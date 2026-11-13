//! Optional, bounded TypeSafe turn classification. Failures leave activity alone.
use anyhow::{Context, Result, ensure};
use mj_core::activity::question_locator;
use mj_core::activity::verdict::{TurnEvidence, api_key, request_fits};
#[cfg(test)]
use mj_core::activity::verdict::{TurnVerdict, questions};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

fn is_glide_size_refusal(status: reqwest::StatusCode, body: &[u8]) -> bool {
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    (status == reqwest::StatusCode::PAYLOAD_TOO_LARGE && body["error"] == "body_too_large")
        // Verified against Fastino with synthetic oversized input, 2026-11-12.
        || (status == reqwest::StatusCode::BAD_REQUEST
            && body["error"]["code"] == "input_too_long")
}

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
                ("bounced", "handback_unfinished") => ("bounced", "Handback was not delivered; the child was asked to continue unfinished assigned work.".into()),
                ("delivered", "handback_accepted") => ("delivered", "Handback check allowed delivery to the parent.".into()),
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
    /// The hosted proxy URL for a GLiDE route. GLiDE is hosted even when the
    /// source is Direct, because the Fastino key exists only as the proxy's secret.
    fn hosted_glide_url(&self, route: &str) -> Result<reqwest::Url> {
        match &self.source {
            VerdictSource::Hosted { endpoint } => Ok(reqwest::Url::parse(endpoint)?.join(route)?),
            VerdictSource::Direct { .. } => Ok(reqwest::Url::parse(
                "https://mj-jev-proxy.eng-admin-a63.workers.dev",
            )?
            .join(route)?),
        }
    }

    /// Compare complete forms. Returns true when `current_question` repeats earlier
    /// ones (hold it for the person) and false when GLiDE finds a new decision
    /// (decline it for now). The request has no size cap and no truncation: an
    /// oversized request or any other failure is an error and the question is held.
    pub(crate) async fn same_afk_question(
        &self,
        session: &str,
        current_question: &serde_json::Value,
        previous_questions: &[serde_json::Value],
    ) -> Result<bool> {
        // The ordinary egress redactor collapses all long identifiers. Keep
        // their equality within this comparison without sending their contents.
        fn anonymize_secret_choices(
            form: &mut serde_json::Value,
            labels: &mut std::collections::BTreeMap<String, usize>,
        ) {
            for field in form["fields"]
                .as_array_mut()
                .expect("normalized form fields")
            {
                // question_evidence has already resolved inherited secrecy.
                if field["secret"] == true
                    && let Some(options) = field.get_mut("options")
                {
                    mj_core::redact::anonymize_json_strings(options, labels);
                }
            }
        }
        let mut labels = std::collections::BTreeMap::new();
        let mut state = serde_json::json!({"current_question": current_question, "previous_questions": previous_questions});
        anonymize_secret_choices(&mut state["current_question"], &mut labels);
        for form in state["previous_questions"]
            .as_array_mut()
            .expect("prior normalized forms")
        {
            anonymize_secret_choices(form, &mut labels);
        }
        mj_core::redact::redact_json_strings_distinct(&mut state, &mut labels);
        let body = mj_core::jev::request_body(&state, None);
        let diagnostic = self.log.as_ref().map(|log| {
            let attempt = log.start(
                session,
                "activity",
                "Does this question repeat one already declined while away?",
                "The question the agent just asked and the earlier questions declined for the same agent.",
            );
            attempt.update(
                None,
                serde_json::json!({"request":body,"contract":"afk-question-glide-v1"}),
            );
            attempt
        });
        let result = self.afk_probabilities(&body).await;
        if let Some(attempt) = &diagnostic {
            match &result {
                Ok((same, different)) => {
                    let new = different > same;
                    attempt.update(
                        None,
                        serde_json::json!({"probabilities":{"same":same,"different":different}}),
                    );
                    if new {
                        attempt.finish(
                            "declined_new",
                            "Classified as a new decision; the worker declines it unless the state changed first.",
                        );
                    } else {
                        attempt.finish("held_repeat", "Held the question for the person.");
                    }
                }
                Err(error) => {
                    attempt.update(None, serde_json::json!({"error":format!("{error:#}")}));
                    attempt.finish(
                        "failed",
                        "The check failed; the question is held for the person.",
                    );
                }
            }
        }
        // A tie does not establish a new decision: the frozen rule is `different`
        // only when its probability is strictly higher.
        result.map(|(same, different)| different <= same)
    }

    /// POST the redacted state to the hosted route and return `(same, different)`.
    async fn afk_probabilities(&self, body: &serde_json::Value) -> Result<(f64, f64)> {
        // Fastino: "set your client read timeout above 170 seconds"
        // https://docs.fastino.ai/troubleshooting/retries (supersedes older 300s notes).
        let response = self
            .client
            .post(self.hosted_glide_url("/v1/afk-question-glide")?)
            .timeout(Duration::from_secs(180))
            .json(body)
            .send()
            .await
            .context("classify AFK question")?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .context("read AFK question verdict")?;
        ensure!(
            bytes.len() <= 64 * 1024,
            "AFK question verdict exceeds proxy response bound"
        );
        ensure!(
            status.is_success(),
            "AFK question HTTP {status}: {}",
            String::from_utf8_lossy(&bytes)
        );
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let scores = value["probabilities"]
            .as_object()
            .context("missing AFK question distribution")?;
        ensure!(scores.len() == 2, "invalid AFK question distribution");
        let probability = |label: &str| -> Result<f64> {
            let p = scores
                .get(label)
                .and_then(serde_json::Value::as_f64)
                .with_context(|| format!("missing {label} probability"))?;
            ensure!(
                p.is_finite() && (0.0..=1.0).contains(&p),
                "invalid AFK question probabilities"
            );
            Ok(p)
        };
        let (same, different) = (probability("same")?, probability("different")?);
        ensure!(
            (same + different - 1.0).abs() <= 0.03,
            "invalid AFK question probabilities"
        );
        Ok((same, different))
    }

    pub(crate) fn log_single_question(
        &self,
        session: &str,
        request: &question_locator::Request,
        stable_id: &str,
    ) {
        if let Some(log) = &self.log {
            let attempt = log.start(
                session,
                "activity",
                "Which reply contains the open question?",
                "Only one assistant reply followed the latest user message.",
            );
            attempt.update(
                None,
                serde_json::json!({"state":mj_core::jev::request_body(&request.state, None),"selected_message":stable_id,"reason":"single_reply"}),
            );
            attempt.finish(
                "applied",
                "Located the only assistant reply without a model request.",
            );
        }
    }
    /// Ask GLiDE which reply holds the open question. The frozen rule picks the
    /// most probable of the replies and `none` (`question_locator::parse`).
    ///
    /// The state builder keeps the request far below the proxy's body bound, so an
    /// explicit size refusal is a failure like any other: no retry, no target stored.
    pub(crate) async fn locate_question(
        &self,
        session: &str,
        request: &question_locator::Request,
    ) -> Result<Option<usize>> {
        let count = request.state.replies.len();
        let body = mj_core::jev::request_body(&request.state, None);
        let diagnostic = self.log.as_ref().map(|log| {
            log.start(
                session,
                "activity",
                "Which reply contains the open question?",
                "Assistant replies since the latest user message.",
            )
        });
        if let Some(log) = &diagnostic {
            log.update(
                None,
                serde_json::json!({"request":body,"contract":"question-locator-glide-v1"}),
            );
        }
        let result = async {
            let endpoint = self.hosted_glide_url("/v1/question-locator-glide")?;
            // Fastino: "set your client read timeout above 170 seconds"
            // https://docs.fastino.ai/troubleshooting/retries (supersedes older 300s notes).
            let response = self
                .client
                .post(endpoint)
                .timeout(Duration::from_secs(180))
                .json(&body)
                .send()
                .await
                .context("request question locator")?;
            let status = response.status();
            let bytes = response.bytes().await.context("read question locator")?;
            ensure!(
                bytes.len() <= 64 * 1024,
                "question locator response exceeds byte limit"
            );
            ensure!(
                status.is_success(),
                "question locator HTTP {status}: {}",
                String::from_utf8_lossy(&bytes)
            );
            let answer: serde_json::Value =
                serde_json::from_slice(&bytes).context("decode locator JSON")?;
            let index = question_locator::parse(&answer["probabilities"], count)?;
            if let Some(log) = &diagnostic {
                log.update(
                    None,
                    serde_json::json!({"answer":answer,"selected_index":index}),
                );
            }
            Ok(index)
        }
        .await;
        if let Some(log) = &diagnostic {
            match &result {
                Ok(Some(_)) => log.finish(
                    "located",
                    "GLiDE selected a reply for the current-assessment check.",
                ),
                Ok(None) => log.finish("unchanged", "No open request was located."),
                Err(error) => {
                    log.update(None, serde_json::json!({"error":format!("{error:#}")}));
                    log.finish(
                        "failed",
                        "The question locator failed; no target was stored.",
                    );
                }
            }
        }
        result
    }
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

    #[cfg(test)]
    fn request_body(
        &self,
        evidence: &TurnEvidence,
        question_set: &serde_json::Value,
    ) -> serde_json::Value {
        let direct_questions = match self.source {
            VerdictSource::Direct { .. } => Some(question_set.clone()),
            VerdictSource::Hosted { .. } => None,
        };
        mj_core::jev::request_body(evidence, direct_questions)
    }

    #[cfg(test)]
    pub(crate) async fn ask_logged(
        &self,
        session: &str,
        generation: u64,
        evidence: &TurnEvidence,
    ) -> (VerdictAttempt, Result<TurnVerdict>) {
        self.ask_logged_for(session, generation, evidence, questions())
            .await
    }

    pub(crate) async fn ask_handback_logged(
        &self,
        session: &str,
        generation: u64,
        evidence: &TurnEvidence,
    ) -> (
        VerdictAttempt,
        Result<mj_core::assessment::HandbackProbabilities>,
    ) {
        let diagnostic = self.log.as_ref().map(|log| {
            log.start(
                session,
                "activity",
                "Does this handback leave assigned work unfinished?",
                "Parent assignment and final report.",
            )
        });
        let body = mj_core::jev::request_body(evidence, None);
        if let Some(log) = &diagnostic {
            log.update(
                None,
                serde_json::json!({"request":body,"contract":"handback-glide-v1",
                "model":"fastino/GLiDE","handback":true,
                "unfinished_share":mj_core::assessment::HANDBACK_UNFINISHED_SHARE,
                "stop_probability_limit":mj_core::assessment::HANDBACK_STOP_PROBABILITY}),
            );
        }
        let attempt = VerdictAttempt {
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
        // User requires fail-open after a few seconds to save the parent's time.
        // Fresh 268-case GLiDE latency: p50 1.1s / p95 2.8s; 5s admits nearly all normal calls.
        let timeout = Duration::from_secs(5);
        let result = tokio::time::timeout(timeout, async {
            // Explicit endpoints support isolated proxy fixtures; no provider key leaves the worker.
            let endpoint = match &self.source {
                VerdictSource::Direct { endpoint, .. } if cfg!(test) => endpoint.clone(),
                VerdictSource::Direct { .. } => {
                    HOSTED_VERDICT_ENDPOINT.replace("/v7/turn-verdict", "/v1/handback-verdict")
                }
                VerdictSource::Hosted { endpoint } => {
                    let mut url = reqwest::Url::parse(endpoint)?;
                    url.set_path("/v1/handback-verdict");
                    url.to_string()
                }
            };
            let response = self
                .post_glide(
                    reqwest::Url::parse(&endpoint)?,
                    evidence
                        .authorization
                        .clone()
                        .context("missing handback authorization")?,
                    timeout,
                    |history| {
                        let mut evidence = evidence.clone();
                        evidence.authorization = Some(history.clone());
                        Ok(mj_core::jev::request_body(&evidence, None))
                    },
                )
                .await?;
            let response = response.json::<serde_json::Value>().await?;
            mj_core::assessment::HandbackProbabilities::parse(&response["probabilities"])
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(std::convert::identity);
        if let Some(log) = &attempt.diagnostic {
            match &result {
                Ok(probabilities) => {
                    log.update(None, serde_json::json!({"probabilities":probabilities}))
                }
                Err(error) => log.update(
                    Some("No usable GLiDE answer; deliver unchecked."),
                    serde_json::json!({"error":format!("{error:#}")}),
                ),
            }
        }
        (attempt, result)
    }

    #[cfg(test)]
    async fn ask_logged_for(
        &self,
        session: &str,
        generation: u64,
        evidence: &TurnEvidence,
        question_set: serde_json::Value,
    ) -> (VerdictAttempt, Result<TurnVerdict>) {
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
            diagnostic.update(None, serde_json::json!({"request":self.request_body(evidence, &question_set), "contract":"turn-verdict-v7", "questions":question_set, "model":"jev-latest", "confidence_threshold":mj_core::activity::verdict::ACT_CONFIDENCE, "no_input_threshold":mj_core::activity::verdict::NO_INPUT_CONFIDENCE, "required_input_probability":mj_core::assessment::REQUIRED_INPUT_PROBABILITY, "required_input_ratio":mj_core::assessment::REQUIRED_INPUT_RATIO, "finished_work_probability":mj_core::assessment::FINISHED_WORK_PROBABILITY, "stale_failure_probability":mj_core::activity::verdict::STALE_FAILURE_PROBABILITY, "stale_reply_probability":mj_core::activity::verdict::STALE_REPLY_PROBABILITY, "stale_finished_silence_s":mj_core::activity::verdict::STALE_FINISHED_SILENCE.as_secs(), "server_retry_threshold":mj_core::activity::verdict::SERVER_RETRY_CONFIDENCE, "generation":generation, "source":if matches!(self.source, VerdictSource::Direct { .. }) { "direct" } else { "hosted" }}));
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
        let result = self.ask(evidence, &question_set).await;
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

    pub(crate) async fn ask_glide_logged(
        &self,
        session: &str,
        generation: u64,
        evidence: &TurnEvidence,
        facts: &mj_core::activity::ActivityFacts,
        native: usize,
    ) -> (VerdictAttempt, Result<glide_turn::Probabilities>) {
        let diagnostic = self.log.as_ref().map(|log| {
            log.start(
                session,
                "activity",
                "Assess the completed turn",
                "Authorization and runtime observations.",
            )
        });
        let attempt = VerdictAttempt {
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
        tracing::info!(target: "mj_jev", session, generation, summary_bytes = evidence.transcript_summary.len(), "GLiDE classification requested");
        let result = async {
            let history = evidence.authorization.clone().context("missing completed-turn authorization")?;
            let body = glide_turn::project(evidence, history.clone(), facts, native)?;
            if let Some(log) = &attempt.diagnostic { log.update(None, serde_json::json!({"request":body,"contract":"glide-turn-v1"})); }
            let answer = self.ask_glide_turn(evidence, history, facts, native).await?;
            if let Some(log) = &attempt.diagnostic { log.update(None, serde_json::json!({"probabilities":answer,"action":mj_core::activity::eot_glide::action(&serde_json::to_value(&answer)?)})); }
            Ok(answer)
        }.await;
        match &result {
            Ok(answer) => {
                tracing::info!(target: "mj_jev", session, generation, probabilities = ?answer, "GLiDE classification received")
            }
            Err(error) => {
                tracing::warn!(target: "mj_jev", session, generation, %error, "GLiDE classification failed")
            }
        }
        (attempt, result)
    }

    /// Retry only an explicit size refusal, and only with a strictly smaller body.
    async fn post_glide(
        &self,
        endpoint: reqwest::Url,
        mut history: mj_core::assessment::ContextHistory,
        timeout: Duration,
        project: impl Fn(&mj_core::assessment::ContextHistory) -> Result<serde_json::Value>,
    ) -> Result<reqwest::Response> {
        let mut body = serde_json::to_vec(&project(&history)?)?;
        loop {
            let response = self
                .client
                .post(endpoint.clone())
                .timeout(timeout)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.clone())
                .send()
                .await
                .context("request GLiDE verdict")?;
            if response.status().is_success() {
                return Ok(response);
            }
            let status = response.status();
            let bytes = response.bytes().await.context("read GLiDE refusal")?;
            if is_glide_size_refusal(status, &bytes) {
                let mut smaller = None;
                while glide_turn::shrink(&mut history) {
                    let candidate = serde_json::to_vec(&project(&history)?)?;
                    if candidate.len() < body.len() {
                        smaller = Some(candidate);
                        break;
                    }
                }
                if let Some(candidate) = smaller {
                    body = candidate;
                    continue;
                }
            }
            anyhow::bail!(
                "GLiDE verdict HTTP {status}: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
    }

    /// Completed-turn transport only; callers retain ownership of action and logging.
    pub(crate) async fn ask_glide_turn(
        &self,
        evidence: &TurnEvidence,
        authorization: mj_core::assessment::ContextHistory,
        facts: &mj_core::activity::ActivityFacts,
        native_agent_count: usize,
    ) -> Result<glide_turn::Probabilities> {
        let endpoint = self.hosted_glide_url("/v1/glide-turn-verdict")?;
        // Fastino: "set your client read timeout above 170 seconds"
        // https://docs.fastino.ai/troubleshooting/retries (supersedes older 300s notes).
        let mut response = self
            .post_glide(
                endpoint,
                authorization,
                Duration::from_secs(180),
                |history| glide_turn::project(evidence, history.clone(), facts, native_agent_count),
            )
            .await?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.context("read GLiDE verdict")? {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= 64 * 1024,
                "GLiDE response exceeds proxy response bound"
            );
            bytes.extend_from_slice(&chunk);
        }
        let probabilities = glide_turn::Probabilities::parse(
            &serde_json::from_slice(&bytes).context("decode GLiDE verdict")?,
        )?;
        if probabilities.attention.is_none() {
            tracing::warn!(
                operation = "glide_turn_verdict",
                "the verdict proxy returned no attention answer; Important attention stays off until the proxy is updated"
            );
        }
        Ok(probabilities)
    }

    #[cfg(test)]
    pub(crate) async fn ask(
        &self,
        evidence: &TurnEvidence,
        question_set: &serde_json::Value,
    ) -> Result<TurnVerdict> {
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
                .json(&self.request_body(evidence, question_set)),
            VerdictSource::Hosted { endpoint } => self
                .client
                .post(endpoint)
                .json(&self.request_body(evidence, question_set)),
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

    /// A hosted-source server for one request; returns the request path and body.
    async fn hosted_server(
        body: String,
    ) -> (
        VerdictClient,
        tokio::task::JoinHandle<(String, serde_json::Value)>,
    ) {
        hosted_server_with_status("200 OK", body).await
    }

    async fn hosted_server_with_status(
        status: &'static str,
        body: String,
    ) -> (
        VerdictClient,
        tokio::task::JoinHandle<(String, serde_json::Value)>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut request_line = String::new();
            socket.read_line(&mut request_line).await.unwrap();
            let path = request_line.split_whitespace().nth(1).unwrap().to_owned();
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
            let mut request = vec![0; length];
            socket.read_exact(&mut request).await.unwrap();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            (path, serde_json::from_slice(&request).unwrap())
        });
        (
            VerdictClient::new(VerdictSource::Hosted { endpoint }).unwrap(),
            task,
        )
    }

    // Hard-won: dc4c34617: a client-side byte cap made long turns permanently unclassifiable
    #[tokio::test]
    async fn glide_handback_large_requests_shrink_only_after_size_refusal() {
        for handback in [false, true] {
            for refusal in [false, true] {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let endpoint = format!("http://{}/", listener.local_addr().unwrap());
                let server = tokio::spawn(async move {
                    let mut sizes = Vec::new();
                    loop {
                        let (socket, _) = listener.accept().await.unwrap();
                        let mut socket = BufReader::new(socket);
                        let mut length = 0usize;
                        loop {
                            let mut line = String::new();
                            socket.read_line(&mut line).await.unwrap();
                            if line == "\r\n" {
                                break;
                            }
                            if let Some(value) =
                                line.to_ascii_lowercase().strip_prefix("content-length:")
                            {
                                length = value.trim().parse().unwrap();
                            }
                        }
                        let mut body = vec![0; length];
                        socket.read_exact(&mut body).await.unwrap();
                        sizes.push(length);
                        let reject = refusal && sizes.len() <= 3;
                        let (status, answer) = if reject {
                            if sizes.len() == 1 {
                                (
                                    "413 Payload Too Large",
                                    serde_json::json!({"error":"body_too_large"}),
                                )
                            } else {
                                (
                                    "400 Bad Request",
                                    serde_json::json!({"error":{"code":"input_too_long","message":"prompt exceeds the maximum context length"}}),
                                )
                            }
                        } else if handback {
                            (
                                "200 OK",
                                serde_json::json!({"probabilities":{"unfinished":0.0,"needs_parent_input":0.0,"blocked":0.0,"permitted_stop":0.0,"finished":1.0,"unclear":0.0}}),
                            )
                        } else {
                            let samples: serde_json::Value =
                                crate::acp::verdict_client::glide_turn::test_samples();
                            ("200 OK", samples[0].clone())
                        };
                        // Hard-won: 8ad2f6a1d: GLiDE inherited Jev's ten-second timeout.
                        if !handback && !refusal {
                            tokio::time::sleep(Duration::from_secs(15)).await;
                        }
                        let answer = answer.to_string();
                        socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len()).as_bytes()).await.unwrap();
                        if !reject {
                            return sizes;
                        }
                    }
                });
                let client = VerdictClient::new(VerdictSource::Hosted { endpoint }).unwrap();
                let mut completed = evidence();
                completed.phase = TurnPhase::Replied;
                completed.authorization = Some(ContextHistory {
                    messages: vec![
                        EvidenceMessage {
                            id: "u".into(),
                            role: "user".into(),
                            text: "Complete task".into(),
                        },
                        EvidenceMessage {
                            id: "a".into(),
                            role: "assistant".into(),
                            text: "synthetic reply ".repeat(14000),
                        },
                    ],
                    ..Default::default()
                });
                if handback {
                    client
                        .ask_handback_logged("isolated", 1, &completed)
                        .await
                        .1
                        .unwrap();
                } else {
                    client
                        .ask_glide_turn(
                            &completed,
                            completed.authorization.clone().unwrap(),
                            &Default::default(),
                            0,
                        )
                        .await
                        .unwrap();
                }
                let sizes = server.await.unwrap();
                assert!(sizes[0] > 200 * 1024);
                assert_eq!(sizes.len(), if refusal { 4 } else { 1 });
                assert!(sizes.windows(2).all(|pair| pair[1] < pair[0]));
            }
        }
    }

    fn evidence() -> TurnEvidence {
        let context = TurnContext::default();
        context.reset(None, "Implement the requested feature");
        context.evidence(
            mj_core::config::HarnessKind::Claude,
            TurnPhase::Running,
            &Default::default(),
            0,
        )
    }

    #[tokio::test]
    async fn locator_sends_redacted_state_alone_to_the_hosted_glide_route() {
        const TOKEN: &str = "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789";
        let reply = serde_json::json!({"probabilities":{"r0":0.2,"r1":0.7,"none":0.1}}).to_string();
        let (client, server) = hosted_server(reply).await;
        let context = ContextHistory {
            messages: vec![
                EvidenceMessage {
                    id: "user:task".into(),
                    role: "user".into(),
                    text: "Task".into(),
                },
                EvidenceMessage {
                    id: "agent:one".into(),
                    role: "assistant".into(),
                    text: format!("Pushed with GH_TOKEN={TOKEN}"),
                },
                EvidenceMessage {
                    id: "agent:two".into(),
                    role: "assistant".into(),
                    text: "Choose?".into(),
                },
            ],
            ..Default::default()
        };
        let request = question_locator::build(&context);
        assert_eq!(
            client.locate_question("session", &request).await.unwrap(),
            Some(1)
        );
        let (path, sent) = server.await.unwrap();
        assert_eq!(path, "/v1/question-locator-glide");
        assert!(!sent.to_string().contains(TOKEN), "{sent}");
        assert_eq!(
            sent["replies"][0]["text"],
            "Pushed with GH_TOKEN=[redacted]"
        );
        assert_eq!(
            sent["replies"][1],
            serde_json::to_value(&request.state.replies[1]).unwrap()
        );
        // The hosted proxy owns the question; the request is the state alone.
        assert_eq!(sent.as_object().unwrap().len(), 2, "{sent}");
    }

    #[tokio::test]
    async fn locator_size_refusal_is_a_failure_without_a_retry() {
        let refusal = serde_json::json!({"error":"body_too_large"}).to_string();
        let (client, server) = hosted_server_with_status("413 Payload Too Large", refusal).await;
        let context = ContextHistory {
            messages: ["a", "b"]
                .iter()
                .map(|id| EvidenceMessage {
                    id: format!("agent:{id}"),
                    role: "assistant".into(),
                    text: "Choose?".into(),
                })
                .collect(),
            ..Default::default()
        };
        let error = client
            .locate_question("session", &question_locator::build(&context))
            .await
            .unwrap_err();
        // The server accepts one connection; a retry would fail to connect instead.
        assert!(format!("{error:#}").contains("413"), "{error:#}");
        server.await.unwrap();
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

    /// The hosted proxy is a third party. Provider errors echo the key that
    /// failed, and agents put tokens on command lines; neither may reach it.
    #[tokio::test]
    async fn hosted_requests_send_only_redacted_evidence_without_authorization() {
        const PROVIDER_KEY: &str = "sk-proj-Q2xhdWRlQ29kZVRlc3RLZXkxMjM0NTY3";
        const INLINE_TOKEN: &str = "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789";
        const BACKGROUND_ID: &str = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
        let mut secret_bearing = evidence();
        secret_bearing.phase = TurnPhase::Replied;
        secret_bearing.completion = Some(mj_core::activity::verdict::CompletionEvidence::bounded(
            "Refusal",
            Some(&mj_core::diagnostic::TurnDiagnostic {
                message: format!("401 Unauthorized: Incorrect API key provided: {PROVIDER_KEY}"),
                code: None,
                http_status: Some(401),
                reset_at: None,
                login_fingerprint: None,
            }),
        ));
        secret_bearing.background = vec![mj_core::activity::verdict::BackgroundEvidence {
            id: BACKGROUND_ID.into(),
            command: format!(
                "curl -H 'Authorization: Bearer {INLINE_TOKEN}' https://api.example.com"
            ),
            started_s_ago: 30,
        }];
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
                let raw = String::from_utf8(request).unwrap();
                assert!(!raw.contains(PROVIDER_KEY), "{raw}");
                assert!(!raw.contains(INLINE_TOKEN), "{raw}");
                let request: serde_json::Value = serde_json::from_str(&raw).unwrap();
                assert_eq!(
                    request["completion"]["diagnostic"]["message"],
                    "401 Unauthorized: Incorrect API key provided: [redacted]"
                );
                assert_eq!(
                    request["background"][0]["command"],
                    "curl -H 'Authorization: Bearer [redacted]' https://api.example.com"
                );
                assert_eq!(request["background"][0]["id"], BACKGROUND_ID);
                assert_eq!(request["user_prompt_tail"], evidence().user_prompt_tail);
                let body = response("background_work");
                socket.write_all(format!("HTTP/1.1 {status} Result\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            });
            let client = VerdictClient::new(VerdictSource::Hosted { endpoint }).unwrap();
            let result = client.ask(&secret_bearing, &questions()).await;
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
        let error = client.ask(&evidence(), &questions()).await.unwrap_err();
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

pub(crate) mod glide_turn;

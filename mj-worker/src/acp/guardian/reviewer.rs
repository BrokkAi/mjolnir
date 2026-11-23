//! The reviewer side of the Mjolnir guardian.
//!
//! A [`Guardian`] belongs to one bridge, and `run_bridge` stops its reviewer
//! after that bridge's own teardown on every exit path. On its first review it
//! starts a second process from the session's launch, retaining the harness,
//! login, and model in a reviewer-specific environment.
//! Each review opens a fresh session there, so no review inherits another's
//! history.
//!
//! The reviewer's launch is narrowed for it (see [`reviewer_launch`]): an
//! OpenCode reviewer gets none of the session's Mjolnir plugins, so it can
//! never drain the session's mailbox, and its hook and rules let it read and
//! nothing else. Every harness's reviewer runs under the session's execution
//! enforcement, its connection advertises no client terminal, file system, or
//! elicitation, Mjolnir passes it no MCP servers, and every permission request
//! it raises is refused once.

use super::*;
use crate::worker_runtime::guardian_endpoint::GuardianEndpoint;
use mj_core::guardian::{
    DaemonGuardianFailure, GUARDIAN_DISABLED_REASON, GuardianReviewOutcome,
    collect_project_instructions, compact_action_summary,
};
use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, PartialEq, Eq)]
enum AcpReviewFailure {
    StartFailed,
    Disconnected,
    ExhaustedAttempts,
    ReviewFailed,
}

impl AcpReviewFailure {
    fn as_str(&self) -> &'static str {
        match self {
            Self::StartFailed => "the ACP reviewer failed to start",
            Self::Disconnected => "the ACP reviewer bridge disconnected",
            Self::ExhaustedAttempts => "the ACP reviewer exhausted its review attempts",
            Self::ReviewFailed => "the ACP reviewer could not complete the review",
        }
    }
}

struct AcpReviewError {
    category: AcpReviewFailure,
    error: anyhow::Error,
}

impl AcpReviewError {
    fn review_failed(error: anyhow::Error) -> Self {
        Self {
            category: AcpReviewFailure::ReviewFailed,
            error,
        }
    }
}

/// Codex's `MAX_REVIEW_ATTEMPTS`: reviewer prompts per review, counting a
/// retry on a fresh reviewer and a reminder for a reply without a verdict.
const MAX_REVIEW_ATTEMPTS: usize = 3;
const STALE_AUTHORIZATION_RETRY_REASON: &str =
    "New user authorization arrived while the previous review was in flight.";
/// How long a retiring reviewer bridge gets to reap its process group after
/// its stdin closes, the grace `run_bridge` gives the session's own bridge.
#[cfg(not(test))]
const REVIEWER_EXIT_GRACE: Duration = Duration::from_secs(5);
#[cfg(test)]
const REVIEWER_EXIT_GRACE: Duration = Duration::from_millis(100);
/// The reviewer's supervisor spec, beside the session's.
#[cfg(unix)]
const REVIEWER_SUPERVISOR_SPEC: &str = "acp-supervisor-guardian.json";

type HostSlot<'a> = tokio::sync::MutexGuard<'a, Option<Arc<ReviewerHost>>>;

/// The guardian for one bridge: the reviewer host it starts on demand and the
/// launch it reviews for.
pub(in crate::acp) struct Guardian {
    spec: LaunchSpec,
    host: tokio::sync::Mutex<Option<Arc<ReviewerHost>>>,
    endpoint: Option<Arc<GuardianEndpoint>>,
    review_sequence: AtomicU64,
}

impl Guardian {
    pub(in crate::acp) fn for_launch(
        spec: &LaunchSpec,
        endpoint: Option<Arc<GuardianEndpoint>>,
    ) -> Option<Arc<Self>> {
        spec.harness
            .mjolnir_guardian_reviews(spec.execution_policy, spec.approval_choices)
            .then(|| {
                Arc::new(Self {
                    spec: spec.clone(),
                    host: tokio::sync::Mutex::new(None),
                    endpoint,
                    review_sequence: AtomicU64::new(0),
                })
            })
    }

    /// Review one permission request.
    pub(in crate::acp) async fn review(
        &self,
        request: &RequestPermissionRequest,
        input: Option<&serde_json::Value>,
        native_child: bool,
        bridge_id: &str,
    ) -> GuardianDecision {
        let deadline = tokio::time::Instant::now() + GUARDIAN_REVIEW_TIMEOUT;
        if self.spec.turn_context.guardian_tripped() {
            tracing::info!(
                target: "mj_guardian",
                tool_call_id = %request.tool_call.tool_call_id,
                "guardian denied several actions this turn; asking the person"
            );
            return GuardianDecision::AskPerson {
                reason: "it denied several actions in this turn".into(),
            };
        }
        let sequence = self.review_sequence.fetch_add(1, Ordering::Relaxed);
        let review_id = format!("{bridge_id}{sequence}");
        let action = planned_action(
            self.spec.harness,
            request,
            input,
            &self.spec.cwd,
            native_child,
        );
        let tool_call_id = request.tool_call.tool_call_id.to_string();
        let cwd = self.spec.cwd.clone();
        let project_instructions = match tokio::time::timeout_at(
            deadline,
            mj_core::runtime::spawn_blocking(move || collect_project_instructions(&cwd)),
        )
        .await
        {
            Ok(Ok(instructions)) => instructions,
            Ok(Err(error)) => {
                tracing::warn!(target: "mj_guardian", %tool_call_id, %error, "could not collect project instructions");
                return GuardianDecision::AskPerson {
                    reason: "it could not collect project instructions".into(),
                };
            }
            Err(_) => {
                tracing::warn!(target: "mj_guardian", %tool_call_id, "collecting project instructions timed out");
                return GuardianDecision::AskPerson {
                    reason: "collecting project instructions timed out".into(),
                };
            }
        };
        if let Some(endpoint) = &self.endpoint {
            match self
                .review_remotely(
                    endpoint,
                    &tool_call_id,
                    &action,
                    project_instructions.as_deref(),
                    deadline,
                )
                .await
            {
                Ok(GuardianReviewOutcome::UseAcpFallback { .. }) => {}
                Ok(GuardianReviewOutcome::Allow { .. }) => {
                    return GuardianDecision::Allow {
                        review: Self::prior_review(
                            review_id,
                            &action,
                            Outcome::Allow,
                            None,
                            None,
                            None,
                        ),
                    };
                }
                Ok(GuardianReviewOutcome::Deny {
                    rationale,
                    risk_level,
                    user_authorization,
                    ..
                }) => {
                    return GuardianDecision::Deny {
                        review: Self::prior_review(
                            review_id,
                            &action,
                            Outcome::Deny,
                            Some(risk_level),
                            Some(user_authorization),
                            Some(&rationale),
                        ),
                        rationale,
                    };
                }
                Ok(GuardianReviewOutcome::AskPerson { reason })
                    if reason == GUARDIAN_DISABLED_REASON =>
                {
                    return GuardianDecision::AskPerson { reason };
                }
                Ok(GuardianReviewOutcome::AskPerson { reason }) => {
                    let reason = DaemonGuardianFailure::from_wire(&reason).as_str();
                    tracing::warn!(target: "mj_guardian", %tool_call_id, %reason, "daemon guardian review failed");
                    return GuardianDecision::AskPerson {
                        reason: format!("its review failed: {reason}"),
                    };
                }
                Err(error) => {
                    tracing::warn!(target: "mj_guardian", %tool_call_id, %error, "guardian relay failed");
                    return GuardianDecision::AskPerson {
                        reason: format!("its review failed: {error}"),
                    };
                }
            }
        }
        self.review_over_acp(
            review_id,
            &tool_call_id,
            &action,
            project_instructions.as_deref(),
        )
        .await
    }

    async fn review_remotely(
        &self,
        endpoint: &GuardianEndpoint,
        tool_call_id: &str,
        action: &serde_json::Value,
        project_instructions: Option<&str>,
        deadline: tokio::time::Instant,
    ) -> Result<GuardianReviewOutcome> {
        let mut evidence = self.collect_evidence(tool_call_id, action, project_instructions);
        loop {
            let result = endpoint.review(evidence.request.clone(), deadline).await?;
            if matches!(
                result.decision,
                GuardianReviewOutcome::UseAcpFallback { .. }
            ) {
                return Ok(result.decision);
            }
            let current = self.collect_evidence(tool_call_id, action, project_instructions);
            if !current.person_wrote_since(&evidence) {
                return Ok(result.decision);
            }
            evidence = current;
            evidence.request.retry_reason = Some(STALE_AUTHORIZATION_RETRY_REASON.into());
            tracing::info!(target: "mj_guardian", "the person wrote during a guardian review; reviewing again");
        }
    }

    async fn review_over_acp(
        &self,
        review_id: String,
        tool_call_id: &str,
        action: &serde_json::Value,
        project_instructions: Option<&str>,
    ) -> GuardianDecision {
        let slot = self.host.lock().await;
        // Time spent waiting for the shared reviewer is outside the review deadline.
        let started = std::time::Instant::now();
        let assessment = match tokio::time::timeout(
            GUARDIAN_REVIEW_TIMEOUT,
            self.decide(slot, tool_call_id, action, project_instructions),
        )
        .await
        {
            Ok(Ok(assessment)) => assessment,
            Ok(Err(failure)) => {
                tracing::warn!(
                    target: "mj_guardian",
                    harness = %self.spec.harness.display_name(),
                    %tool_call_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    error = %format!("{:#}", failure.error),
                    "ACP guardian review failed"
                );
                return GuardianDecision::AskPerson {
                    reason: format!("its review failed: {}", failure.category.as_str()),
                };
            }
            Err(_) => {
                tracing::warn!(
                    target: "mj_guardian",
                    harness = %self.spec.harness.display_name(),
                    %tool_call_id,
                    "ACP guardian review timed out"
                );
                return GuardianDecision::AskPerson {
                    reason: format!(
                        "the ACP review did not finish within {} seconds",
                        GUARDIAN_REVIEW_TIMEOUT.as_secs()
                    ),
                };
            }
        };
        let outcome = assessment.enforced_outcome();
        // Never the rationale or the reviewer's text: both can quote
        // workspace content. The transcript carries a denial's rationale.
        tracing::info!(
            target: "mj_guardian",
            harness = %self.spec.harness.display_name(),
            %tool_call_id,
            ?outcome,
            reviewer_outcome = ?assessment.outcome,
            risk_level = ?assessment.risk_level,
            user_authorization = ?assessment.user_authorization,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "guardian reviewed a permission request"
        );
        let review = Self::prior_review(
            review_id,
            action,
            outcome,
            Some(assessment.risk_level),
            Some(assessment.user_authorization),
            Some(&assessment.rationale),
        );
        match outcome {
            Outcome::Allow => GuardianDecision::Allow { review },
            Outcome::Deny => GuardianDecision::Deny {
                rationale: assessment.rationale,
                review,
            },
        }
    }

    fn prior_review(
        review_id: String,
        action: &serde_json::Value,
        outcome: Outcome,
        risk_level: Option<mj_core::guardian::RiskLevel>,
        user_authorization: Option<mj_core::guardian::UserAuthorization>,
        rationale: Option<&str>,
    ) -> PriorGuardianReview {
        use mj_core::assessment::trim_middle;

        PriorGuardianReview {
            review_id: Some(review_id),
            tool: trim_middle(action["tool"].as_str().unwrap_or("tool")),
            action_summary: compact_action_summary(action),
            outcome,
            risk_level,
            user_authorization,
            rationale: rationale
                .filter(|reason| !reason.trim().is_empty())
                .map(trim_middle),
        }
    }

    pub(in crate::acp) fn record_settled_review(
        &self,
        review: PriorGuardianReview,
        journal: mpsc::Permit<'_, RuntimeEvent>,
    ) -> bool {
        let denied = review.outcome == Outcome::Deny;
        self.spec
            .turn_context
            .record_prior_guardian_review(review.clone());
        journal.send(RuntimeEvent::GuardianReviewed { review });
        self.spec.turn_context.record_guardian_review(denied)
    }

    fn collect_evidence(
        &self,
        tool_call_id: &str,
        action: &serde_json::Value,
        project_instructions: Option<&str>,
    ) -> ReviewEvidence {
        ReviewEvidence::collect(
            self.spec.harness,
            &self.spec.turn_context,
            tool_call_id,
            action,
            project_instructions.map(str::to_owned),
        )
    }

    /// Review `action` against the evidence as it stands. If the person wrote
    /// while it was reviewed, the verdict may rest on stale authorization, so
    /// it is reviewed again on what they now say, as Codex discards a stale
    /// verdict (`review_request.rs`). Each repeat follows a new message; the
    /// review deadline bounds them.
    async fn decide(
        &self,
        first: HostSlot<'_>,
        tool_call_id: &str,
        action: &serde_json::Value,
        project_instructions: Option<&str>,
    ) -> Result<Assessment, AcpReviewError> {
        let mut slot = Some(first);
        let mut attempts = 0;
        let mut evidence = self.collect_evidence(tool_call_id, action, project_instructions);
        loop {
            let prompt = vec![
                ContentBlock::Text(TextContent::new(reviewer_instructions_acp())),
                ContentBlock::Text(TextContent::new(evidence.request.request_text())),
            ];
            let assessment = self.assess(&mut slot, &prompt, &mut attempts).await?;
            let current = self.collect_evidence(tool_call_id, action, project_instructions);
            if !current.person_wrote_since(&evidence) {
                return Ok(assessment);
            }
            evidence = current;
            evidence.request.retry_reason = Some(STALE_AUTHORIZATION_RETRY_REASON.into());
            tracing::info!(
                target: "mj_guardian",
                "the person wrote during a guardian review; reviewing again"
            );
        }
    }

    /// Run the review's attempts and return the first verdict. Every retry
    /// follows a change: a reviewer whose process died is replaced, and a
    /// reply without a verdict is asked for only the verdict. Any other
    /// failure would repeat, so it ends the review.
    async fn assess(
        &self,
        slot: &mut Option<HostSlot<'_>>,
        prompt: &[ContentBlock],
        attempts: &mut usize,
    ) -> Result<Assessment, AcpReviewError> {
        loop {
            let host = {
                let mut guard = match slot.take() {
                    Some(guard) => guard,
                    None => self.host.lock().await,
                };
                self.current_host(&mut guard)
                    .await
                    .map_err(|error| AcpReviewError {
                        category: AcpReviewFailure::StartFailed,
                        error,
                    })?
            };
            // Count a host that exits before the review future is polled.
            *attempts += 1;
            let attempt = host
                .until_exit(self.assess_on(&host, prompt, attempts))
                .await;
            let failure = match attempt {
                Ok(assessment) => return Ok(assessment),
                Err(failure) => failure,
            };
            if failure.category == AcpReviewFailure::ExhaustedAttempts || host.alive() {
                return Err(failure);
            }
            if *attempts >= MAX_REVIEW_ATTEMPTS {
                return Err(AcpReviewError {
                    category: AcpReviewFailure::Disconnected,
                    error: failure.error,
                });
            }
            tracing::debug!(
                target: "mj_guardian",
                error = %format!("{:#}", failure.error),
                "guardian reviewer exited; retrying on a fresh one"
            );
            self.discard(&host).await;
        }
    }

    /// One reviewer session's attempts: the prompt, then the verdict reminder
    /// while replies carry none and attempts remain.
    async fn assess_on(
        &self,
        host: &Arc<ReviewerHost>,
        prompt: &[ContentBlock],
        attempts: &mut usize,
    ) -> Result<Assessment, AcpReviewError> {
        let session = host
            .open_session(&self.spec)
            .await
            .map_err(AcpReviewError::review_failed)?;
        let mut blocks = prompt.to_vec();
        loop {
            let reply = session
                .prompt(blocks)
                .await
                .map_err(AcpReviewError::review_failed)?;
            match parse_assessment(&reply) {
                Ok(assessment) => return Ok(assessment),
                Err(error) if *attempts < MAX_REVIEW_ATTEMPTS => {
                    *attempts += 1;
                    tracing::debug!(
                        target: "mj_guardian",
                        reply_bytes = reply.len(),
                        %error,
                        "guardian reply carried no verdict; asking again"
                    );
                    blocks = vec![ContentBlock::Text(TextContent::new(VERDICT_REMINDER))];
                }
                Err(error) => {
                    return Err(AcpReviewError {
                        category: AcpReviewFailure::ExhaustedAttempts,
                        error: error.context(format!(
                            "the reviewer replied {} bytes without a verdict",
                            reply.len()
                        )),
                    });
                }
            }
        }
    }

    /// The live reviewer in `slot`, started now if there is none.
    async fn current_host(
        &self,
        slot: &mut Option<Arc<ReviewerHost>>,
    ) -> Result<Arc<ReviewerHost>> {
        if let Some(host) = slot.as_ref().filter(|host| host.alive()) {
            return Ok(host.clone());
        }
        if let Some(dead) = slot.take() {
            dead.shutdown().await;
        }
        let host = ReviewerHost::start(&self.spec).await?;
        *slot = Some(host.clone());
        Ok(host)
    }

    /// Forget `host` if it is still the current reviewer, and stop it.
    async fn discard(&self, host: &Arc<ReviewerHost>) {
        let mut slot = self.host.lock().await;
        if slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, host))
        {
            slot.take();
        }
        drop(slot);
        host.shutdown().await;
    }

    /// Stop the reviewer with the bridge it belongs to.
    pub(in crate::acp) async fn shutdown(&self) {
        let host = self.host.lock().await.take();
        if let Some(host) = host {
            host.shutdown().await;
        }
    }
}

/// How the reviewer's bridge is started.
struct ReviewerLaunch {
    command: PathBuf,
    args: Vec<String>,
    environment: BTreeMap<String, String>,
    profile: Option<tempfile::TempDir>,
}

/// The session's launch, narrowed for a reviewer. A production bridge is the
/// worker's ACP supervisor, which takes the harness environment from its spec
/// file, so the reviewer gets a spec of its own beside the session's.
fn reviewer_launch(spec: &LaunchSpec) -> Result<ReviewerLaunch> {
    #[cfg(unix)]
    if let Some(path) = &spec.bridge_spec_path {
        let mut supervisor = crate::worker_runtime::AcpSupervisorSpec::read(path)?;
        supervisor.environment = reviewer_environment(spec.harness, &supervisor.environment)?;
        supervisor.excluded_environment.extend(
            reviewer_profile::SESSION_ENVIRONMENT
                .iter()
                .map(|name| (*name).to_owned()),
        );
        let profile = reviewer_profile::stage(
            spec.harness,
            &mut supervisor.environment,
            &mut supervisor.args,
        )?;
        let reviewer_path = path.with_file_name(REVIEWER_SUPERVISOR_SPEC);
        supervisor.write_spec(&reviewer_path)?;
        let mut args = spec.args.clone();
        let value = args
            .iter()
            .position(|arg| arg == "--spec")
            .map(|flag| flag + 1)
            .filter(|value| *value < args.len())
            .context("the session's bridge command names no supervisor spec")?;
        args[value] = reviewer_path
            .to_str()
            .context("reviewer supervisor spec path is not UTF-8")?
            .to_owned();
        return Ok(ReviewerLaunch {
            command: spec.command.clone(),
            args,
            environment: {
                let mut environment = spec.environment.clone();
                reviewer_profile::remove_session_environment(&mut environment);
                environment
            },
            profile,
        });
    }
    let environment = reviewer_environment(spec.harness, &spec.environment)?;
    #[cfg(unix)]
    let (environment, args, profile) = {
        let mut environment = environment;
        let mut args = spec.args.clone();
        let profile = reviewer_profile::stage(spec.harness, &mut environment, &mut args)?;
        (environment, args, profile)
    };
    #[cfg(not(unix))]
    let (args, profile) = (spec.args.clone(), None);
    Ok(ReviewerLaunch {
        command: spec.command.clone(),
        args,
        environment,
        profile,
    })
}

/// The harness environment a reviewer runs with.
fn reviewer_environment(
    harness: HarnessKind,
    environment: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    let mut environment = environment.clone();
    reviewer_profile::remove_session_environment(&mut environment);
    match harness {
        #[cfg(unix)]
        HarnessKind::OpenCode => crate::worker_runtime::opencode_reviewer_environment(&environment),
        #[cfg(not(unix))]
        HarnessKind::OpenCode => bail!("an OpenCode reviewer needs a Unix worker"),
        HarnessKind::Codex => {
            let mut config: serde_json::Map<String, serde_json::Value> = environment
                .get("CODEX_CONFIG")
                .map(|config| serde_json::from_str(config))
                .transpose()
                .context("parse the Codex reviewer configuration")?
                .unwrap_or_default();
            config.remove("hooks");
            config.remove("mcp_servers");
            config.remove("sandbox_mode");
            config.insert("default_permissions".into(), ":read-only".into());
            config.insert("web_search".into(), "disabled".into());
            let features = config
                .entry("features")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .context("Codex reviewer features must be an object")?;
            features.remove("codex_hooks");
            for key in ["hooks", "apps", "plugins", "remote_plugin"] {
                features.insert(key.into(), false.into());
            }
            environment.insert("CODEX_CONFIG".into(), serde_json::to_string(&config)?);
            Ok(environment)
        }
        _ => Ok(environment),
    }
}

/// The text each reviewer session has sent as its latest agent message.
#[derive(Default)]
struct Replies(Mutex<HashMap<String, ReviewerReply>>);

#[derive(Default)]
struct ReviewerReply {
    message_id: Option<String>,
    text: String,
}

impl Replies {
    fn observe(&self, session_id: &str, update: &serde_json::Value) {
        let mut replies = self.0.lock().expect("guardian replies lock poisoned");
        let Some(reply) = replies.get_mut(session_id) else {
            return;
        };
        match update
            .get("sessionUpdate")
            .and_then(serde_json::Value::as_str)
        {
            Some("agent_message_chunk") => {
                let message_id = update
                    .get("messageId")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned);
                // Only the latest message is the reply (muse-acp does the same).
                if message_id != reply.message_id {
                    reply.text.clear();
                    reply.message_id = message_id;
                }
                if let Some(text) = update
                    .get("content")
                    .filter(|content| {
                        content.get("type").and_then(serde_json::Value::as_str) == Some("text")
                    })
                    .and_then(|content| content.get("text"))
                    .and_then(serde_json::Value::as_str)
                {
                    reply.text.push_str(text);
                }
            }
            // A tool call ends the message before it; the verdict follows.
            Some("tool_call") => {
                reply.text.clear();
                reply.message_id = None;
            }
            _ => {}
        }
    }

    fn open(&self, session_id: &str) {
        self.0
            .lock()
            .expect("guardian replies lock poisoned")
            .insert(session_id.to_owned(), ReviewerReply::default());
    }

    fn take(&self, session_id: &str) -> String {
        self.0
            .lock()
            .expect("guardian replies lock poisoned")
            .get_mut(session_id)
            .map(|reply| {
                reply.message_id = None;
                std::mem::take(&mut reply.text)
            })
            .unwrap_or_default()
    }

    fn close(&self, session_id: &str) {
        self.0
            .lock()
            .expect("guardian replies lock poisoned")
            .remove(session_id);
    }
}

/// A running reviewer bridge and its ACP connection.
struct ReviewerHost {
    connection: ConnectionTo<Agent>,
    supports_close: bool,
    replies: Arc<Replies>,
    stop: CancellationToken,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Cancelled once the bridge or its connection is gone, by whichever
    /// observer sees it first (see [`ReviewerHost::until_exit`]). The one
    /// owner of whether this reviewer is alive: a request to a dead bridge
    /// may never be answered, so every request races it.
    gone: CancellationToken,
}

impl ReviewerHost {
    async fn start(spec: &LaunchSpec) -> Result<Arc<Self>> {
        let launch = {
            let spec = spec.clone();
            mj_core::runtime::spawn_blocking(move || reviewer_launch(&spec))
                .await
                .context("prepare the reviewer's launch task failed")??
        };
        let mut command = Command::new(&launch.command);
        command
            .args(&launch.args)
            .env_clear()
            .envs(&launch.environment)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Register the bridge with the worker's orphan reaper as it spawns,
        // so the reaper never takes the exit status its owner awaits.
        let mut child =
            mj_core::subprocess::spawn_async_child(&mut command).with_context(|| {
                format!(
                    "start the reviewer's {} bridge {}",
                    spec.harness.display_name(),
                    launch.command.display()
                )
            })?;
        let stdin = child
            .stdin
            .take()
            .context("reviewer bridge stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("reviewer bridge stdout unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("reviewer bridge stderr unavailable")?;
        let transport =
            ByteStreams::new(stdin.compat_write(), skip_stdout_preamble(stdout).compat());
        let replies = Arc::new(Replies::default());
        let notification_replies = replies.clone();
        let stop = CancellationToken::new();
        // Until the host exists, a start that is dropped (its review was
        // cancelled or timed out) must still stop the bridge it spawned.
        let stop_on_drop = stop.clone().drop_guard();
        let connection_stop = stop.clone();
        let task_stop = stop.clone();
        let gone = CancellationToken::new();
        let task_gone = gone.clone();
        let harness = spec.harness;
        let (connection_tx, connection_rx) = oneshot::channel();
        let profile = launch.profile;
        let task = mj_core::runtime::spawn(async move {
            let _profile = profile;
            let stderr_tail = mj_core::runtime::spawn(read_stderr_tail(stderr));
            let (reaped, ended) = {
                let run = Client
                    .v1()
                    .on_receive_notification(
                        async move |notification: RawSessionNotification, _cx| {
                            notification_replies.observe(
                                &notification.session_id.to_string(),
                                &notification.update,
                            );
                            Ok(())
                        },
                        agent_client_protocol::on_receive_notification!(),
                    )
                    .on_receive_request(
                        async move |request: RequestPermissionRequest, responder, _cx| {
                            tracing::debug!(
                                target: "mj_guardian",
                                tool_call_id = %request.tool_call.tool_call_id,
                                "refused a guardian reviewer's tool request"
                            );
                            responder.respond(rejection_response(&request))
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    // The reviewer gets no client terminal, file system, or
                    // elicitation. Every request is answered, because the ACP
                    // crate parks an unhandled one that carries a session id.
                    .on_receive_request(
                        async move |request: UntypedMessage, responder, _cx| {
                            responder.respond_with_error(
                                agent_client_protocol::Error::method_not_found()
                                    .data(serde_json::Value::String(request.method().to_owned())),
                            )
                        },
                        agent_client_protocol::on_receive_request!(),
                    )
                    .connect_with(transport, async move |connection: ConnectionTo<Agent>| {
                        if connection_tx.send(connection).is_ok() {
                            connection_stop.cancelled().await;
                        }
                        Ok(())
                    });
                tokio::pin!(run);
                // A start can be dropped before the bridge opens its ACP
                // connection, when `connection_stop` does not exist yet.
                // Observe the host's owner directly so a hung bridge is
                // closed, killed if necessary, and reaped.
                tokio::select! {
                    result = &mut run => (false, result.err().map(|error| error.to_string())),
                    status = child.wait() => (true, Some(match status {
                        Ok(status) => format!("exited with {status}"),
                        Err(error) => format!("could not be waited for: {error}"),
                    })),
                    () = task_stop.cancelled() => (false, Some("was stopped before its connection opened".into())),
                }
            };
            task_gone.cancel();
            // The transport is gone, so the supervisor's stdin is closed: give
            // it the session bridge's grace to reap its process group, then
            // kill it.
            if !reaped
                && tokio::time::timeout(REVIEWER_EXIT_GRACE, child.wait())
                    .await
                    .is_err()
            {
                if let Err(error) = child.kill().await {
                    tracing::warn!(target: "mj_guardian", %error, "could not kill the guardian reviewer bridge");
                }
                if let Err(error) = child.wait().await {
                    tracing::warn!(target: "mj_guardian", %error, "could not reap the guardian reviewer bridge");
                }
            }
            let tail = match stderr_tail.await {
                Ok(Ok(tail)) => tail,
                Ok(Err(error)) => format!("(stderr unreadable: {error:#})"),
                Err(error) => format!("(stderr task failed: {error})"),
            };
            // An exit nobody asked for is a reviewer failure; its stderr says
            // why (a login that lapsed, a harness that would not start).
            if !task_stop.is_cancelled() {
                tracing::warn!(
                    target: "mj_guardian",
                    harness = %harness.display_name(),
                    ended = ended.as_deref().unwrap_or("connection closed"),
                    stderr = %actionable_stderr_tail(&tail).unwrap_or_default(),
                    "guardian reviewer bridge stopped unexpectedly"
                );
            }
        });
        let connection = match connection_rx.await {
            Ok(connection) => connection,
            Err(_) => {
                if let Err(error) = task.await {
                    tracing::warn!(target: "mj_guardian", %error, "guardian reviewer task failed");
                }
                bail!("the reviewer's bridge closed its connection before it opened");
            }
        };
        let initialized = tokio::select! {
            initialized = initialize_reviewer(&connection, spec) => initialized,
            () = gone.cancelled() => Err(anyhow!("the reviewer's bridge exited during initialization")),
        };
        let supports_close = match initialized {
            Ok(supports_close) => supports_close,
            Err(error) => {
                drop(stop_on_drop);
                if let Err(join_error) = task.await {
                    tracing::warn!(target: "mj_guardian", error = %join_error, "guardian reviewer task failed");
                }
                return Err(error);
            }
        };
        stop_on_drop.disarm();
        Ok(Arc::new(Self {
            connection,
            supports_close,
            replies,
            stop,
            task: Mutex::new(Some(task)),
            gone,
        }))
    }

    fn alive(&self) -> bool {
        !self.gone.is_cancelled()
    }

    /// Run `work` against this reviewer, failing it if the bridge exits.
    ///
    /// A request can learn that the bridge is gone before this host's task
    /// does: the connection fails it with "incoming transport closed" first.
    /// That failure is recorded in `gone` before it is returned, so the
    /// decision to retry, which reads only `gone`, sees the death whichever
    /// observer saw it first.
    async fn until_exit<T>(
        &self,
        work: impl Future<Output = Result<T, AcpReviewError>>,
    ) -> Result<T, AcpReviewError> {
        let result = tokio::select! {
            result = work => result,
            () = self.gone.cancelled() => Err(AcpReviewError {
                category: AcpReviewFailure::Disconnected,
                error: anyhow!("the reviewer's bridge exited"),
            }),
        };
        if let Err(error) = &result
            && error.error.chain().any(|cause| {
                cause
                    .downcast_ref::<agent_client_protocol::Error>()
                    .is_some_and(agent_client_protocol::is_incoming_transport_closed)
            })
        {
            self.gone.cancel();
        }
        result
    }

    /// Open a reviewer session configured like the reviewed one.
    async fn open_session(self: &Arc<Self>, spec: &LaunchSpec) -> Result<ReviewerSession> {
        let mut request = NewSessionRequest::new(spec.cwd.clone()).mcp_servers(Vec::new());
        if spec.harness == HarnessKind::Claude {
            request = request.meta(serde_json::Map::from_iter([(
                "claudeCode".into(),
                serde_json::json!({
                    "options": {
                        "settingSources": ["user"],
                        "settings": {"disableAllHooks": true},
                        "strictMcpConfig": true,
                        "tools": ["Read", "Glob", "Grep"]
                    }
                }),
            )]));
        }
        let created = self
            .connection
            .send_request(request)
            .block_task()
            .await
            .context("open a reviewer session")?;
        let session = ReviewerSession {
            host: self.clone(),
            id: created.session_id,
            prompting: AtomicBool::new(false),
        };
        self.replies.open(&session.id.to_string());
        let mut options = created.config_options.unwrap_or_default();
        let mut modes = created.modes;
        // The reviewed session's policy, so whatever would ask there asks
        // here and is refused.
        if let Some(refusal) = reassert_execution_policy(
            &self.connection,
            &session.id,
            spec.harness,
            session_enforcement(spec),
            &mut options,
            &mut modes,
            PolicySync::Always,
        )
        .await?
        {
            return Err(refusal
                .error
                .context("the reviewer session refused the session's execution policy"));
        }
        // The session's model reviews, never silently another one: a session
        // whose harness accepted a model selection must find that model here.
        let selectable = find_session_config_option(&options, "model").is_some();
        match spec.startup_model() {
            Some(model) if selectable => {
                if session_model(&options).as_deref() != Some(model.as_str()) {
                    set_session_config(
                        &self.connection,
                        &session.id,
                        &mut options,
                        spec.harness,
                        "model",
                        &model,
                    )
                    .await
                    .with_context(|| {
                        format!("select the session's model {model:?} for the reviewer")
                    })?;
                }
            }
            Some(model)
                if spec
                    .accepted_config
                    .lock()
                    .expect("accepted session configuration lock poisoned")
                    .model
                    .is_some() =>
            {
                bail!(
                    "the reviewer's {} session offers no model selector, so it cannot run on the session's model {model:?}",
                    spec.harness.display_name()
                );
            }
            _ => {}
        }
        // A review is a bounded judgement: Codex and muse-acp both run their
        // reviewer at low effort where the model offers it.
        if selector_value_is_offered(&options, "effort", "low") {
            set_session_config(
                &self.connection,
                &session.id,
                &mut options,
                spec.harness,
                "effort",
                "low",
            )
            .await
            .context("select low effort for the reviewer")?;
        }
        Ok(session)
    }

    async fn shutdown(&self) {
        self.stop.cancel();
        let task = self
            .task
            .lock()
            .expect("guardian task lock poisoned")
            .take();
        if let Some(task) = task
            && let Err(error) = task.await
        {
            tracing::warn!(target: "mj_guardian", %error, "guardian reviewer task failed");
        }
    }
}

/// Initialize ACP without client terminal, file system, or elicitation.
/// Returns whether the bridge can close sessions.
async fn initialize_reviewer(connection: &ConnectionTo<Agent>, spec: &LaunchSpec) -> Result<bool> {
    let initialized = connection
        .send_request(
            InitializeRequest::new(ProtocolVersion::V1)
                .client_info(
                    Implementation::new(CLIENT_NAME, env!("CARGO_PKG_VERSION"))
                        .title("Mjolnir guardian"),
                )
                .client_capabilities(ClientCapabilities::new()),
        )
        .block_task()
        .await
        .with_context(|| {
            format!(
                "initialize the reviewer's {} bridge",
                spec.harness.display_name()
            )
        })?;
    ensure!(
        initialized.protocol_version == ProtocolVersion::V1,
        "the reviewer's bridge negotiated unsupported protocol {:?}",
        initialized.protocol_version
    );
    Ok(initialized
        .agent_capabilities
        .session_capabilities
        .close
        .is_some())
}

impl Drop for ReviewerHost {
    /// A host dropped without [`ReviewerHost::shutdown`] still stops: its
    /// task closes the transport and reaps the bridge, logging what fails.
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// One review's session on the reviewer bridge. Dropping it cancels a turn
/// still running, as a cancelled or timed-out review leaves one, and closes
/// the session in the background, outside any review's deadline.
struct ReviewerSession {
    host: Arc<ReviewerHost>,
    id: SessionId,
    prompting: AtomicBool,
}

impl ReviewerSession {
    /// Send one prompt and return the reviewer's latest message.
    async fn prompt(&self, blocks: Vec<ContentBlock>) -> Result<String> {
        self.prompting.store(true, Ordering::Release);
        let response = self
            .host
            .connection
            .send_request(PromptRequest::new(self.id.clone(), blocks))
            .block_task()
            .await
            .context("run the reviewer's turn")?;
        self.prompting.store(false, Ordering::Release);
        let reply = self.host.replies.take(&self.id.to_string());
        ensure!(
            response.stop_reason == StopReason::EndTurn,
            "the reviewer's turn ended with {:?}",
            response.stop_reason
        );
        Ok(reply)
    }
}

impl Drop for ReviewerSession {
    fn drop(&mut self) {
        self.host.replies.close(&self.id.to_string());
        if !self.host.alive() {
            return;
        }
        if self.prompting.load(Ordering::Acquire)
            && let Err(error) = self
                .host
                .connection
                .send_notification(CancelNotification::new(self.id.clone()))
        {
            tracing::debug!(target: "mj_guardian", %error, "could not cancel an abandoned guardian review");
        }
        if self.host.supports_close
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let connection = self.host.connection.clone();
            let id = self.id.clone();
            runtime.spawn(async move {
                if let Err(error) = connection
                    .send_request(CloseSessionRequest::new(id))
                    .block_task()
                    .await
                {
                    tracing::debug!(target: "mj_guardian", %error, "could not close a guardian reviewer session");
                }
            });
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::test_support::wait_for;

    fn is_running(pid: i32) -> bool {
        // SAFETY: signal 0 only checks whether the recorded child still exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    // Hard-won: 747336f25: a reviewer that hung before opening ACP survived cancellation.
    #[tokio::test]
    async fn dropping_a_started_reviewer_before_connection_reaps_its_bridge() {
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("hung-reviewer.py");
        let pid_file = temp.path().join("pid");
        std::fs::write(
            &script,
            r#"
import os, sys, time
with open(sys.argv[1], "w") as out:
    out.write(str(os.getpid()))
while True:
    time.sleep(1)
"#,
        )
        .unwrap();
        let mut spec = crate::acp::tests::silent_bridge_spec(mj_core::activity::StallPolicy {
            silence: None,
            tool_call: None,
        });
        spec.command = "python3".into();
        spec.args = vec![
            script.to_string_lossy().into_owned(),
            pid_file.to_string_lossy().into_owned(),
        ];
        spec.cwd = temp.path().to_owned();
        spec.harness = HarnessKind::Kimi;

        let mut start = Box::pin(ReviewerHost::start(&spec));
        let pid = wait_for("the hung reviewer bridge to start", async {
            loop {
                tokio::select! {
                    result = &mut start => {
                        panic!("reviewer start returned before the bridge connected: {}", result.is_err())
                    }
                    () = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
                if let Ok(pid) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = pid.parse::<i32>()
                {
                    break pid;
                }
            }
        })
        .await;
        drop(start);

        wait_for("the cancelled reviewer bridge to be reaped", async {
            while is_running(pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    }
}

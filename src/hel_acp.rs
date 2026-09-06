//! ACP runtime and normalized session controls used by a Hel session worker.
//!
//! The worker owns exactly one harness process and one foreground session.  It
//! deliberately does not know about orchestration, review lanes, or subagents;
//! [`surface`] projects protocol capabilities for the chat control surface.

mod dialect;
#[cfg(test)]
mod plan_tests;
#[cfg(test)]
mod session_config_tests;
pub mod step_clock;
pub mod surface;
mod terminal_compat;
pub use step_clock::StepClock;
pub use surface::PlanControl;
pub use terminal_compat::fallback_terminal_tool_call;
pub(crate) use terminal_compat::fallback_terminal_tool_call_id;
pub use terminal_compat::is_fallback_terminal_tool_call;

use dialect::grok;

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::TextContent;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ClientCapabilities, CloseSessionRequest, ContentBlock,
    CreateTerminalRequest, CreateTerminalResponse, ElicitationCapabilities,
    ElicitationFormCapabilities, Implementation, InitializeRequest, KillTerminalRequest,
    KillTerminalResponse, LoadSessionRequest, McpServer, McpServerStdio, NewSessionRequest,
    PermissionOptionKind, PromptRequest, PromptResponse, ReleaseTerminalRequest,
    ReleaseTerminalResponse, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectOptions, SessionConfigValueId, SessionId,
    SessionModeState, SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionModeRequest, StopReason, TerminalExitStatus, TerminalId, TerminalOutputRequest,
    TerminalOutputResponse, ToolCallUpdateFields, WaitForTerminalExitRequest,
    WaitForTerminalExitResponse,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectTo, ConnectionTo, UntypedMessage};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::hel_config::{ExecutionEnforcement, ExecutionPolicy, HarnessKind};
use crate::hel_elicitation::{
    ElicitationField, ElicitationFieldKind, ElicitationOption, ElicitationRequest,
    ElicitationResponse, ElicitationValue,
};
use crate::hel_terminal::{
    DEFAULT_TERMINAL_OUTPUT_BYTES, TerminalExit, TerminalRegistry, TerminalSpawn,
};
use crate::hel_worker::{AcpActivityClock, ClaimedSteeringPrompt};
use crate::hel_worker_launch::{ProjectMemoryLaunchConfig, ProjectMemoryMcpDelivery};

pub fn plan_review_carries_native_feedback(id: &str) -> bool {
    grok::is_plan_review_id(id)
}

/// Identity prefix every normalized plan decision shares, whatever harness
/// dialect produced it.
pub const PLAN_REVIEW_ID_PREFIX: &str = "plan-review-";

/// Header [`normalized_plan_review`] puts in front of the harness's proposal
/// text. Reading the proposal back out is the inverse, so both live here.
const PLAN_REVIEW_MESSAGE_PREFIX: &str = "Review the agent's plan:\n\n";

/// Whether this elicitation id belongs to one of Hel's normalized plan
/// decisions.
#[must_use]
pub fn is_plan_review_id(id: &str) -> bool {
    id.starts_with(PLAN_REVIEW_ID_PREFIX)
}

/// The exact proposal text a normalized plan decision carries.
///
/// Returns `None` for any other elicitation, and for a plan decision whose
/// message was not built by [`normalized_plan_review`].
#[must_use]
pub fn plan_review_proposal(request: &ElicitationRequest) -> Option<&str> {
    if !is_plan_review_id(&request.id) {
        return None;
    }
    request.message.strip_prefix(PLAN_REVIEW_MESSAGE_PREFIX)
}

/// The plan decision Hel answers itself instead of forwarding to the harness.
/// Every other decision maps to a native option through the dialect bridge.
pub const PLAN_REVIEW_SECOND_OPINION: &str = "second_opinion";

/// The proposal to review when this answer asked for a second opinion.
///
/// A second opinion is local: the harness's decision stays pending while Hel
/// sets the reviewer up, so this answer must never reach ACP. Callers use the
/// returned proposal as the captured text they hand to the reviewer.
#[must_use]
pub fn plan_review_second_opinion<'a>(
    request: &'a ElicitationRequest,
    response: &ElicitationResponse,
) -> Option<&'a str> {
    let proposal = plan_review_proposal(request)?;
    let ElicitationResponse::Accept { content } = response else {
        return None;
    };
    match content.get(PLAN_REVIEW_ACTION) {
        Some(ElicitationValue::String(action)) if action == PLAN_REVIEW_SECOND_OPINION => {
            Some(proposal)
        }
        _ => None,
    }
}

/// The answer Hel gives the harness once a second opinion has been set up.
///
/// Gathering context needs an idle planning session, so the pending decision
/// has to be resolved first. Declining keeps plan mode active, which is why
/// the captured proposal is the only copy of the plan that survives and why
/// cancelling a review owes the user a Hel-owned decision in its place.
#[must_use]
pub fn plan_review_keep_planning() -> ElicitationResponse {
    ElicitationResponse::Accept {
        content: std::collections::BTreeMap::from([(
            PLAN_REVIEW_ACTION.to_owned(),
            ElicitationValue::String("keep_planning".to_owned()),
        )]),
    }
}

/// Private ACP metadata is provider-local and has no Hel projection. In
/// particular, Codex can replay terminal-output metadata for old tool calls on
/// every `session/load`; journaling those invisible deltas grows the relay and
/// makes every later recovery replay them again.
fn session_update_is_relay_visible(
    update: &SessionUpdate,
    live_tool_calls: &Mutex<BTreeSet<String>>,
    session_id: &str,
) -> bool {
    match update {
        SessionUpdate::ToolCall(call) => {
            live_tool_calls
                .lock()
                .expect("live ACP tool-call set lock poisoned")
                .insert(call.tool_call_id.to_string());
            true
        }
        SessionUpdate::ToolCallUpdate(update)
            if update.fields == ToolCallUpdateFields::default() =>
        {
            false
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let created_live = live_tool_calls
                .lock()
                .expect("live ACP tool-call set lock poisoned")
                .contains(&update.tool_call_id.to_string());
            if !created_live {
                tracing::warn!(
                    %session_id,
                    tool_call_id = %update.tool_call_id,
                    "ignored delayed ACP update for a tool call not created on this live connection"
                );
            }
            created_live
        }
        _ => true,
    }
}

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub command: PathBuf,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub project_memory: Option<ProjectMemoryLaunchConfig>,
    /// Extra stdio MCP servers this session gets, beyond project memory. A
    /// turn review's reviewing agents get Bifrost this way; the primary
    /// session gets none.
    pub extra_mcp_servers: Vec<crate::hel_worker_launch::ReviewMcpServer>,
    pub resume_session: Option<String>,
    /// Accepted selectors for this logical session, shared across native
    /// bridge replacements. Workers seed this from their durable relay.
    pub accepted_config: Arc<Mutex<AcceptedSessionConfig>>,
    pub harness: HarnessKind,
    pub execution_policy: ExecutionPolicy,
    pub acp_activity: AcpActivityClock,
    /// When the step the agent is on began. Marked from the same handlers as
    /// `acp_activity`, but only where a new step actually starts.
    pub step_clock: StepClock,
}

/// Only model and reasoning effort survive a bridge replacement. Restoring
/// plan/permission modes here could override the current execution policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AcceptedSessionConfig {
    model: Option<String>,
    effort: Option<String>,
}

impl AcceptedSessionConfig {
    pub fn from_configuration(
        values: &BTreeMap<String, String>,
        options: &[SessionConfigOption],
    ) -> Self {
        let accepted = |key: &str| {
            let option = find_session_config_option(options, key);
            let recorded = values
                .get(key)
                .or_else(|| option.and_then(|option| values.get(&option.id.to_string())))?;
            Some(recorded.clone())
        };
        Self {
            model: accepted("model"),
            effort: accepted("effort"),
        }
    }

    fn remember(&mut self, key: &str, value: &str, options: &[SessionConfigOption]) -> bool {
        let is_selector = |canonical: &str| {
            key == canonical
                || find_session_config_option(options, canonical)
                    .is_some_and(|option| option.id.to_string() == key)
        };
        let current = |canonical| {
            let option = find_session_config_option(options, canonical)?;
            let SessionConfigKind::Select(select) = &option.kind else {
                return None;
            };
            Some(select.current_value.to_string())
        };
        if is_selector("model") {
            self.model = Some(value.to_owned());
            // A model change can reset effort or remove that selector.
            self.effort = current("effort");
        } else if is_selector("effort") {
            self.effort = Some(value.to_owned());
            if let Some(model) = current("model") {
                self.model = Some(model);
            }
        } else {
            return false;
        }
        true
    }

    /// Fold a completed selector command into the durable configuration using
    /// the same accepted pair as the live bridge. Startup advertisements alone
    /// must never replace it with the provider's defaults.
    pub(crate) fn record_completed(
        values: &mut BTreeMap<String, String>,
        key: &str,
        value: &str,
        options: &[SessionConfigOption],
    ) {
        let mut accepted = Self::from_configuration(values, options);
        if !accepted.remember(key, value, options) {
            return;
        }
        for canonical in ["model", "effort"] {
            values.remove(canonical);
            if let Some(option) = find_session_config_option(options, canonical) {
                values.remove(&option.id.to_string());
            }
        }
        if let Some(model) = accepted.model {
            values.insert("model".into(), model);
        }
        if let Some(effort) = accepted.effort {
            values.insert("effort".into(), effort);
        }
    }
}

fn project_memory_mcp(spec: &LaunchSpec) -> Vec<McpServer> {
    if spec.harness == HarnessKind::Claude
        || spec
            .project_memory
            .as_ref()
            .is_some_and(|memory| memory.mcp_delivery == ProjectMemoryMcpDelivery::HarnessProfile)
    {
        return Vec::new();
    }
    let Some(memory) = &spec.project_memory else {
        return Vec::new();
    };
    vec![McpServer::Stdio(
        McpServerStdio::new("mj-project-memory", spec.command.clone()).args(vec![
            "worker".into(),
            "memory-mcp".into(),
            "--root".into(),
            memory.root.to_string_lossy().into_owned(),
        ]),
    )]
}

fn session_request_meta(spec: &LaunchSpec) -> Option<serde_json::Map<String, serde_json::Value>> {
    (spec.harness == HarnessKind::Claude && spec.execution_policy.is_unconstrained()).then(|| {
        let serde_json::Value::Object(meta) = serde_json::json!({
            "claudeCode": {
                "options": {
                    "sandbox": {
                        "enabled": false
                    }
                }
            }
        }) else {
            unreachable!("Claude session metadata is an object")
        };
        meta
    })
}

/// The reviewing agents' analyzer servers, for harnesses that accept a server
/// over ACP. Claude and Kimi read their staged profile instead, which the
/// controller writes while staging the reviewer.
fn extra_mcp(spec: &LaunchSpec) -> Vec<McpServer> {
    if crate::hel_worker_launch::ReviewMcpDelivery::for_harness(spec.harness)
        != crate::hel_worker_launch::ReviewMcpDelivery::Acp
    {
        return Vec::new();
    }
    spec.extra_mcp_servers
        .iter()
        .map(|server| {
            McpServer::Stdio(
                McpServerStdio::new(server.name.clone(), server.command.clone())
                    .args(server.args.clone()),
            )
        })
        .collect()
}

fn new_session_request(spec: &LaunchSpec, include_project_memory: bool) -> NewSessionRequest {
    let request = NewSessionRequest::new(spec.cwd.clone())
        .additional_directories(spec.additional_directories.clone())
        .meta(session_request_meta(spec));
    let mut servers = extra_mcp(spec);
    if include_project_memory {
        servers.extend(project_memory_mcp(spec));
    }
    if servers.is_empty() {
        request
    } else {
        request.mcp_servers(servers)
    }
}

fn load_session_request(spec: &LaunchSpec, session_id: SessionId) -> LoadSessionRequest {
    LoadSessionRequest::new(session_id, spec.cwd.clone())
        .additional_directories(spec.additional_directories.clone())
        // Loading must preserve the native session's original MCP set. Adding
        // Hel's current project-memory server here mutates an existing Codex
        // session and can make its history replay emit updates for tools whose
        // creation was never part of this relay stream. New sessions receive
        // the server above; resumed sessions keep whatever they began with.
        .meta(session_request_meta(spec))
}

#[derive(Debug)]
pub enum CommandRequest {
    Prompt {
        request_id: String,
        prompt: Vec<ContentBlock>,
    },
    SetConfig {
        request_id: String,
        key: String,
        value: String,
    },
    /// Select an ACP session mode through `session/set_mode`.
    SetSessionMode {
        request_id: String,
        mode_id: String,
    },
    /// Connection-only answer to an in-flight ACP elicitation. The content is
    /// deliberately never put in the durable relay command ledger.
    ResolveElicitation {
        elicitation_id: String,
        response: ElicitationResponse,
        resolved: oneshot::Sender<std::result::Result<(), String>>,
    },
    Cancel {
        request_id: String,
        steering_prompt: Option<ClaimedSteeringPrompt>,
    },
    Close {
        request_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    Connected {
        agent_name: Option<String>,
        agent_version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        protocol_version: Option<ProtocolVersion>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capabilities: Option<Box<AgentCapabilities>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_info: Option<Implementation>,
    },
    SessionStarted {
        native_session_id: String,
        resumed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_mode: Option<String>,
    },
    SessionConfigured {
        config_options: Vec<SessionConfigOption>,
    },
    SessionModesConfigured {
        modes: Option<SessionModeState>,
    },
    SessionUpdate {
        update: serde_json::Value,
    },
    ElicitationRequested {
        request: ElicitationRequest,
    },
    ElicitationResolved {
        elicitation_id: String,
        action: String,
    },
    PromptFinished {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        request_id: String,
        stop_reason: String,
    },
    Warning {
        message: String,
    },
    /// A client terminal started successfully. The worker records an interim
    /// tool call so agents that omit the ACP association do not strand its
    /// eventual result as a standalone transcript item.
    TerminalStarted {
        terminal_id: String,
        command: String,
        started_at_ms: i64,
    },
    /// A client-run terminal was reaped. Exactly one of these is emitted per
    /// terminal, by the supervisor that waits on the child, so kill and
    /// release flow through the same report.
    TerminalClosed {
        terminal_id: String,
        output: String,
        #[serde(default)]
        truncated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<String>,
    },
    UserShellOutput {
        request_id: String,
        command: String,
        stdout: String,
        stderr: String,
        stdout_truncated: bool,
        stderr_truncated: bool,
    },
    UserShellFinished {
        request_id: String,
        result: crate::hel_worker::UserShellResult,
    },
    ConfigApplied {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        request_id: String,
        key: String,
        value: String,
        /// The complete configuration returned by ACP for this change. Keep
        /// it in the same runtime event as command completion so the relay
        /// cannot publish a checkpoint between the two durable observations.
        #[serde(default)]
        config_options: Vec<SessionConfigOption>,
    },
    SessionModeApplied {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        request_id: String,
        mode_id: String,
        #[serde(default)]
        config_options: Vec<SessionConfigOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        modes: Option<SessionModeState>,
    },
    CommandRejected {
        request_id: String,
        message: String,
    },
    CommandInterrupted {
        request_id: String,
        message: String,
    },
    CancelApplied {
        request_id: String,
    },
    SteerApplied {
        request_id: String,
        queued_command_id: String,
    },
    CloseApplied {
        request_id: String,
    },
    /// The ACP child died or the protocol broke after a session was open.
    /// The coordinator interrupts in-flight commands; the runtime reloads the
    /// native session on a new bridge instead of stopping the worker.
    HarnessRestarting {
        message: String,
    },
    Stopped,
}

type PendingElicitations = Arc<Mutex<BTreeMap<String, oneshot::Sender<ElicitationResponse>>>>;

/// A permission callback captures the current command's sender, so a late
/// answer cannot attach an implementation to a subsequent prompt or bridge.
type PlanImplementationSlot = Arc<Mutex<Option<mpsc::UnboundedSender<PlanImplementation>>>>;

struct PlanImplementation {
    plan: String,
    permission_sent: oneshot::Receiver<bool>,
}

struct ActivePlanImplementation(PlanImplementationSlot);

type ActivePrompt = Pin<
    Box<
        dyn Future<Output = std::result::Result<PromptResponse, agent_client_protocol::Error>>
            + Send,
    >,
>;

struct RestoredPlanMode {
    config_options: Vec<SessionConfigOption>,
    modes: Option<SessionModeState>,
    plan: String,
}

type PlanModeRestoration<'a> = Pin<Box<dyn Future<Output = Result<RestoredPlanMode>> + Send + 'a>>;

async fn restore_plan_execution_mode(
    connection: &ConnectionTo<Agent>,
    session_id: SessionId,
    mut state: RestoredPlanMode,
    permission_sent: oneshot::Receiver<bool>,
) -> Result<RestoredPlanMode> {
    ensure!(
        permission_sent.await.unwrap_or(false),
        "Claude's plan permission response could not be delivered"
    );
    enforce_execution_mode(
        connection,
        &session_id,
        "bypassPermissions",
        &mut state.config_options,
        &mut state.modes,
    )
    .await?;
    for option in &state.config_options {
        if option.category == Some(SessionConfigOptionCategory::Mode)
            && let SessionConfigKind::Select(select) = &option.kind
        {
            ensure!(
                select.current_value.to_string() == "bypassPermissions",
                "Claude did not apply the required bypassPermissions mode"
            );
        }
    }
    Ok(state)
}

impl Drop for ActivePlanImplementation {
    fn drop(&mut self) {
        self.0
            .lock()
            .expect("plan implementation lock poisoned")
            .take();
    }
}

enum PlanPermissionAnswer {
    Native(RequestPermissionResponse),
    ContinueInBypass,
}

fn policy_plan_permission_answer(
    request: &RequestPermissionRequest,
    response: ElicitationResponse,
    harness: HarnessKind,
    policy: ExecutionPolicy,
) -> Result<PlanPermissionAnswer> {
    if harness != HarnessKind::Claude || plan_review_answer(response.clone()).0 != "implement" {
        return Ok(PlanPermissionAnswer::Native(permission_plan_response(
            request, response,
        )));
    }
    let (mode, ids) = if policy.is_unconstrained() {
        (
            "bypassPermissions",
            ["bypassPermissions", "exit-plan-bypass"],
        )
    } else {
        ("auto", ["auto", "exit-plan-auto"])
    };
    if let Some(option) = request.options.iter().find(|option| {
        ids.contains(&option.option_id.to_string().as_str())
            && matches!(
                option.kind,
                PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
            )
    }) {
        return Ok(PlanPermissionAnswer::Native(
            RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new(option.option_id.clone()),
            )),
        ));
    }
    if policy.is_unconstrained() {
        Ok(PlanPermissionAnswer::ContinueInBypass)
    } else {
        bail!(
            "Cannot implement the approved plan: Claude did not offer the required {mode} mode. Update the Claude bridge or use a model supporting Auto mode."
        )
    }
}

pub async fn run(
    spec: LaunchSpec,
    requests: mpsc::Receiver<CommandRequest>,
    events: mpsc::Sender<RuntimeEvent>,
) -> Result<()> {
    let result = run_inner(spec, requests, events.clone()).await;
    if let Err(error) = &result {
        emit_runtime_event(
            &events,
            RuntimeEvent::Warning {
                message: format!("ACP runtime failed: {error:#}"),
            },
        )
        .await
        .with_context(|| format!("report ACP runtime failure: {error:#}"))?;
    }
    emit_runtime_event(&events, RuntimeEvent::Stopped).await?;
    result
}

#[derive(Clone)]
struct OpenedSession {
    native_session_id: String,
    started_at: tokio::time::Instant,
}

struct BridgeRestart {
    native_session_id: String,
    unexpected: bool,
    session_age: Duration,
    message: &'static str,
}

async fn run_inner(
    mut spec: LaunchSpec,
    mut requests: mpsc::Receiver<CommandRequest>,
    events: mpsc::Sender<RuntimeEvent>,
) -> Result<()> {
    let mut rapid_deaths = 0_u32;
    let mut replacing_previous_bridge = false;
    loop {
        let opened = Arc::new(Mutex::new(None));
        match run_bridge(
            &spec,
            &mut requests,
            &events,
            opened.clone(),
            replacing_previous_bridge,
        )
        .await?
        {
            None => return Ok(()),
            Some(restart) => {
                if restart.unexpected {
                    if restart.session_age < RAPID_BRIDGE_WINDOW {
                        rapid_deaths += 1;
                        ensure!(
                            rapid_deaths < RAPID_BRIDGE_RESTART_LIMIT,
                            "ACP bridge exited repeatedly during startup; giving up"
                        );
                    } else {
                        rapid_deaths = 0;
                    }
                }
                emit_runtime_event(
                    &events,
                    RuntimeEvent::HarnessRestarting {
                        message: restart.message.to_owned(),
                    },
                )
                .await?;
                spec.resume_session = Some(restart.native_session_id);
                replacing_previous_bridge = true;
            }
        }
    }
}

/// Run one ACP bridge process. `Some` means reload the native session on a
/// fresh bridge: a cancel that never acked, or a dead ACP child.
async fn run_bridge(
    spec: &LaunchSpec,
    requests: &mut mpsc::Receiver<CommandRequest>,
    events: &mpsc::Sender<RuntimeEvent>,
    opened: Arc<Mutex<Option<OpenedSession>>>,
    replacing_previous_bridge: bool,
) -> Result<Option<BridgeRestart>> {
    let mut child = Command::new(&spec.command)
        .args(&spec.args)
        .envs(&spec.environment)
        .current_dir(&spec.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("launch ACP bridge {}", spec.command.display()))?;
    let stdin = child.stdin.take().context("ACP bridge stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("ACP bridge stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("ACP bridge stderr unavailable")?;
    let stderr_task = tokio::spawn(read_stderr_tail(stderr));
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());

    let (mut result, child_reaped) = {
        let drive = drive(
            transport,
            spec.clone(),
            requests,
            events.clone(),
            opened.clone(),
            replacing_previous_bridge,
        );
        tokio::pin!(drive);
        tokio::select! {
            biased;
            result = &mut drive => (result, false),
            waited = child.wait() => {
                let result = match waited {
                    Ok(status) => Err(anyhow!(
                        "ACP bridge exited before the protocol runtime completed with {status}; \
                         bridge stdout must contain only JSON-RPC frames and login-shell startup must be silent"
                    )),
                    Err(error) => Err(error).context("wait for ACP bridge"),
                };
                (result, true)
            }
        }
    };
    let opened_now = opened.lock().expect("opened session lock poisoned").clone();
    let restarting = matches!(&result, Ok(Some(_))) || (result.is_err() && opened_now.is_some());
    // Dropping the transport closes the supervisor's stdin. Give it time to
    // terminate and reap the complete bridge process group before killing the
    // supervisor itself as a last resort. A planned restart already decided
    // to kill the child, so a non-zero exit is the expected outcome.
    if !child_reaped {
        if restarting {
            if let Err(error) = child.kill().await {
                tracing::warn!(
                    operation = "acp_bridge_restart",
                    %error,
                    "could not kill ACP bridge during planned restart"
                );
            }
            if let Err(error) = child.wait().await {
                tracing::warn!(
                    operation = "acp_bridge_restart",
                    %error,
                    "could not reap ACP bridge during planned restart"
                );
            }
        } else {
            let cleanup =
                match tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await {
                    Ok(Ok(status)) if status.success() => Ok(()),
                    Ok(Ok(status)) => Err(anyhow!(
                        "ACP bridge exited with {status} after the protocol runtime completed"
                    )),
                    Ok(Err(error)) => Err(error).context("wait for ACP bridge shutdown"),
                    Err(_) => {
                        let killed = child.kill().await.context("kill unresponsive ACP bridge");
                        let waited = child
                            .wait()
                            .await
                            .context("reap killed ACP bridge")
                            .map(|_| ());
                        match (killed, waited) {
                            (Ok(()), Ok(())) => Ok(()),
                            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
                            (Err(error), Err(wait_error)) => Err(error.context(format!(
                                "also failed to reap killed ACP bridge: {wait_error:#}"
                            ))),
                        }
                    }
                };
            if let Err(error) = cleanup {
                merge_drive_error(&mut result, error);
            }
        }
    }
    let stderr_tail = match stderr_task.await {
        Ok(Ok(tail)) => tail,
        Ok(Err(error)) => {
            merge_drive_error(&mut result, error);
            String::new()
        }
        Err(error) => {
            merge_drive_error(
                &mut result,
                anyhow!("ACP stderr collector task failed: {error}"),
            );
            String::new()
        }
    };
    if !restarting && let Some(stderr_tail) = actionable_stderr_tail(&stderr_tail) {
        result =
            result.map_err(|error| error.context(format!("ACP bridge stderr:\n{stderr_tail}")));
    }
    match result {
        Ok(None) => Ok(None),
        Ok(Some(native_session_id)) => Ok(Some(BridgeRestart {
            native_session_id,
            unexpected: false,
            session_age: opened_now
                .map(|opened| opened.started_at.elapsed())
                .unwrap_or(Duration::ZERO),
            message: ACP_BRIDGE_RESTART_WARNING,
        })),
        Err(error) => match opened_now {
            None => Err(error),
            Some(opened) => Ok(Some(BridgeRestart {
                native_session_id: opened.native_session_id,
                unexpected: true,
                session_age: opened.started_at.elapsed(),
                message: ACP_BRIDGE_LOST_WARNING,
            })),
        },
    }
}

const ACP_STDERR_TAIL_BYTES: usize = 16 * 1024;
const UNEXPECTED_PERMISSION_REQUEST_WARNING: &str = "The agent made a permission request while configured to run unconstrained; its execution policy is misconfigured.";
/// Chatter the Claude bridge logs for SDK events it does not model, for example
/// `Unexpected case: {"type":"vcs_state_changed"}`. It arrives often enough to
/// fill the whole stderr tail and bury the real failure in worker exit records.
const ADAPTER_CHATTER_PREFIX: &str = "Unexpected case: ";
/// Kimi 0.37.x logs this for response-shaped startup frames with a null id.
/// It is adapter routing noise and commonly precedes a useful ACP error.
const KIMI_NULL_RESPONSE_CHATTER: &str = "Got response to unknown request null";

const PLAN_REVIEW_ACTION: &str = "action";
const PLAN_REVIEW_FEEDBACK: &str = "feedback";

fn nested_string<'a>(value: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    match value {
        serde_json::Value::Object(object) => {
            for key in keys {
                if let Some(value) = object.get(*key).and_then(serde_json::Value::as_str) {
                    return Some(value);
                }
            }
            object.values().find_map(|value| nested_string(value, keys))
        }
        serde_json::Value::Array(values) => {
            values.iter().find_map(|value| nested_string(value, keys))
        }
        _ => None,
    }
}

fn nested_string_matches(
    value: &serde_json::Value,
    keys: &[&str],
    predicate: &impl Fn(&str) -> bool,
) -> bool {
    match value {
        serde_json::Value::Object(object) => object.iter().any(|(key, value)| {
            (keys.contains(&key.as_str()) && value.as_str().is_some_and(predicate))
                || nested_string_matches(value, keys, predicate)
        }),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| nested_string_matches(value, keys, predicate)),
        _ => false,
    }
}

fn is_plan_permission(request: &RequestPermissionRequest) -> bool {
    let Ok(value) = serde_json::to_value(request) else {
        return false;
    };
    // Claude Code's ExitPlanMode approval arrives as a `switch_mode` tool call
    // whose rawInput carries the plan text and a `planFilePath`; its title is
    // "Ready to code?" and its options are generic permission-mode ids
    // (`default`, `acceptEdits`, `plan`, ...). None of those match a title or
    // option-id heuristic, so key on the tool kind and the plan payload.
    nested_string_matches(&value, &["kind"], &|kind| {
        kind == "plan_review" || kind == "switch_mode"
    }) || nested_string(&value, &["planFilePath", "plan_file_path"]).is_some()
        || nested_string_matches(&value, &["title", "name"], &|name| {
            let normalized = name.to_ascii_lowercase().replace([' ', '_'], "");
            normalized.contains("implementthisplan") || normalized.contains("exitplanmode")
        })
        || request.options.iter().any(|option| {
            let id = option.option_id.to_string().to_ascii_lowercase();
            id.contains("plan_approve")
                || id.contains("implement_plan")
                || id.contains("plan_revise")
                || id.contains("reject_and_exit")
        })
}

pub fn normalized_plan_review(id: String, value: &serde_json::Value) -> ElicitationRequest {
    let plan = nested_string(value, &["plan", "plan_content", "planContent"])
        .unwrap_or("The agent did not provide plan text in its review request.");
    ElicitationRequest {
        id,
        title: Some("Plan review".into()),
        message: format!("{PLAN_REVIEW_MESSAGE_PREFIX}{plan}"),
        description: Some("Choose what Mjolnir should tell the planning harness.".into()),
        fields: vec![
            ElicitationField {
                id: PLAN_REVIEW_ACTION.into(),
                title: "Decision".into(),
                description: Some(
                    "Implement approves the plan; revise sends the feedback below.".into(),
                ),
                required: true,
                secret: false,
                custom_answer_for: None,
                custom_answer_option: None,
                kind: ElicitationFieldKind::SingleSelect {
                    options: vec![
                        ElicitationOption {
                            value: "implement".into(),
                            title: "Implement".into(),
                            description: Some("Approve and continue with implementation".into()),
                            preview: None,
                        },
                        ElicitationOption {
                            value: "revise".into(),
                            title: "Revise".into(),
                            description: Some("Keep planning and incorporate feedback".into()),
                            preview: None,
                        },
                        ElicitationOption {
                            value: PLAN_REVIEW_SECOND_OPINION.into(),
                            title: "Get a second opinion".into(),
                            description: Some(
                                "Ask another agent to review this plan before you decide".into(),
                            ),
                            preview: None,
                        },
                        ElicitationOption {
                            value: "keep_planning".into(),
                            title: "Keep planning".into(),
                            description: Some("Decline this plan without leaving plan mode".into()),
                            preview: None,
                        },
                        ElicitationOption {
                            value: "exit".into(),
                            title: "Exit plan mode".into(),
                            description: Some(
                                "Abandon this review and return to normal mode".into(),
                            ),
                            preview: None,
                        },
                    ],
                    default: Some("keep_planning".into()),
                },
            },
            ElicitationField {
                id: PLAN_REVIEW_FEEDBACK.into(),
                title: "Revision feedback".into(),
                description: Some("Describe what the agent should change.".into()),
                required: false,
                secret: false,
                custom_answer_for: Some(PLAN_REVIEW_ACTION.into()),
                custom_answer_option: Some("revise".into()),
                kind: ElicitationFieldKind::Text {
                    default: None,
                    min_length: None,
                    max_length: Some(16 * 1024),
                    pattern: None,
                    format: None,
                },
            },
        ],
    }
}

fn plan_review_answer(response: ElicitationResponse) -> (String, Option<String>) {
    let ElicitationResponse::Accept { content } = response else {
        return ("keep_planning".into(), None);
    };
    let action = match content.get(PLAN_REVIEW_ACTION) {
        Some(ElicitationValue::String(action)) => action.clone(),
        _ => "keep_planning".into(),
    };
    let feedback = match content.get(PLAN_REVIEW_FEEDBACK) {
        Some(ElicitationValue::String(feedback)) if !feedback.trim().is_empty() => {
            Some(feedback.clone())
        }
        _ => None,
    };
    (action, feedback)
}

fn permission_plan_response(
    request: &RequestPermissionRequest,
    response: ElicitationResponse,
) -> RequestPermissionResponse {
    let (action, _) = plan_review_answer(response);
    let needles: &[&str] = match action.as_str() {
        "implement" => &["implement_plan", "plan_approve", "default", "approve"],
        "revise" => &["plan_revise", "revise"],
        "exit" => &["reject_and_exit", "exit"],
        // A second opinion is answered locally and never reaches here. If one
        // ever did, it must not approve the plan, so it declines like every
        // other non-approval and leaves the session in plan mode.
        _ => &[],
    };
    let selected = request
        .options
        .iter()
        .find(|option| {
            let id = option.option_id.to_string().to_ascii_lowercase();
            let name = option.name.to_ascii_lowercase();
            needles
                .iter()
                .any(|needle| id.contains(needle) || name.contains(needle))
        })
        .or_else(|| {
            // No harness-specific option id matched. Claude's "Ready to code?"
            // exposes only generic kinds, so fall back by intent: implement
            // takes an allow option; every decline (revise, keep_planning,
            // exit) takes a reject option to stay in plan mode rather than
            // cancelling the turn.
            if action == "implement" {
                // Prefer the least-privileged approval so an unmatched harness
                // never silently escalates to a bypass-permissions option.
                request
                    .options
                    .iter()
                    .find(|option| option.kind == PermissionOptionKind::AllowOnce)
                    .or_else(|| {
                        request
                            .options
                            .iter()
                            .find(|option| option.kind == PermissionOptionKind::AllowAlways)
                    })
            } else {
                request
                    .options
                    .iter()
                    .find(|option| option.kind == PermissionOptionKind::RejectOnce)
                    .or_else(|| {
                        request
                            .options
                            .iter()
                            .find(|option| option.kind == PermissionOptionKind::RejectAlways)
                    })
            }
        });
    selected.map_or_else(
        || RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
        |option| {
            RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new(option.option_id.clone()),
            ))
        },
    )
}

fn unsupported_client_request_report(method: &str) -> String {
    format!(
        "The agent sent the client request {method}, which Hel does not implement. \
         Hel answered with a method-not-found error rather than leaving the agent waiting."
    )
}

/// The part of a bridge stderr tail worth attaching to a failing result.
/// Returns `None` when only adapter chatter was captured, so a failure keeps
/// its own error text instead of gaining misleading context.
fn actionable_stderr_tail(tail: &str) -> Option<String> {
    let kept = tail
        .lines()
        .filter(|line| {
            let line = line.trim();
            !line.starts_with(ADAPTER_CHATTER_PREFIX) && line != KIMI_NULL_RESPONSE_CHATTER
        })
        .collect::<Vec<_>>()
        .join("\n");
    let kept = kept.trim();
    (!kept.is_empty()).then(|| kept.to_owned())
}

async fn emit_runtime_event(
    events: &mpsc::Sender<RuntimeEvent>,
    event: RuntimeEvent,
) -> Result<()> {
    events
        .send(event)
        .await
        .map_err(|_| anyhow!("relay event coordinator stopped"))
}

/// Answer for a `terminal/*` request naming a terminal this connection does
/// not have, most often one the agent already released.
fn unknown_terminal_error(terminal_id: &str) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(serde_json::Value::String(format!(
        "unknown terminal {terminal_id}"
    )))
}

fn terminal_exit_status(exit: &TerminalExit) -> TerminalExitStatus {
    TerminalExitStatus::new()
        .exit_code(exit.exit_code)
        .signal(exit.signal.clone())
}

fn relay_event_channel_error() -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(serde_json::Value::String(
        "relay event coordinator stopped".into(),
    ))
}

fn merge_drive_error(result: &mut Result<Option<String>>, additional: anyhow::Error) {
    let previous = std::mem::replace(result, Ok(None));
    *result = match previous {
        Ok(_) => Err(additional),
        Err(error) => Err(error.context(format!("additional ACP runtime error: {additional:#}"))),
    };
}

async fn read_stderr_tail(mut stderr: tokio::process::ChildStderr) -> Result<String> {
    let mut tail = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        match stderr.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                tail.extend_from_slice(&buffer[..read]);
                if tail.len() > ACP_STDERR_TAIL_BYTES {
                    tail.drain(..tail.len() - ACP_STDERR_TAIL_BYTES);
                }
            }
            Err(error) => {
                return Err(error).context("read ACP bridge stderr");
            }
        }
    }
    Ok(String::from_utf8_lossy(&tail).trim().to_owned())
}

/// How long a `session/cancel` may take to settle `session/prompt` before Hel
/// kills the bridge and reloads the native session. A cooperative cancel can
/// flush thinking; this bound is for the case that never acks.
const CANCEL_ACK_TIMEOUT: Duration = Duration::from_secs(60);

const CANCEL_UNACKED_WARNING: &str =
    "cancel was not acknowledged within 60s; restarting the harness";

const ACP_BRIDGE_LOST_WARNING: &str = "ACP bridge exited; reloading the native session";
const ACP_BRIDGE_RESTART_WARNING: &str = "ACP bridge restarting; reloading the native session";

/// Give up if a freshly opened session dies this many times in a row before it
/// has lived for [`RAPID_BRIDGE_WINDOW`]. A later crash of a healthy session
/// resets the count.
const RAPID_BRIDGE_RESTART_LIMIT: u32 = 3;
const RAPID_BRIDGE_WINDOW: Duration = Duration::from_secs(5);

async fn drive<T>(
    transport: T,
    spec: LaunchSpec,
    requests: &mut mpsc::Receiver<CommandRequest>,
    events: mpsc::Sender<RuntimeEvent>,
    opened: Arc<Mutex<Option<OpenedSession>>>,
    replacing_previous_bridge: bool,
) -> Result<Option<String>>
where
    T: ConnectTo<Client>,
{
    let notification_events = events.clone();
    let notification_activity = spec.acp_activity.clone();
    let notification_step_clock = spec.step_clock.clone();
    let session_update_count = Arc::new(AtomicU64::new(0));
    let notification_session_update_count = session_update_count.clone();
    // A provider may replay the native transcript as `session/update`
    // notifications while answering `session/load`. Hel already owns that
    // history in its durable relay, so accepting the replay would duplicate
    // every old turn on every restart. New sessions have no old history.
    let session_updates_enabled = Arc::new(AtomicBool::new(spec.resume_session.is_none()));
    let notification_session_updates_enabled = session_updates_enabled.clone();
    // Codex can finish dispatching old tool updates after `session/load` has
    // already returned. Track only creations observed after the load boundary,
    // so those delayed updates cannot reintroduce historical tool state into
    // the durable relay. A live tool always announces its creation before its
    // updates on the same ACP connection.
    let live_tool_calls = Arc::new(Mutex::new(BTreeSet::<String>::new()));
    let notification_live_tool_calls = live_tool_calls.clone();
    let permission_events = events.clone();
    let permission_activity = spec.acp_activity.clone();
    let permission_step_clock = spec.step_clock.clone();
    let ext_events = events.clone();
    let ext_activity = spec.acp_activity.clone();
    let ext_step_clock = spec.step_clock.clone();
    let ext_harness = spec.harness;
    let elicitation_events = events.clone();
    let pending_elicitations = PendingElicitations::default();
    let handler_elicitations = pending_elicitations.clone();
    let permission_elicitations = pending_elicitations.clone();
    let permission_review_ids = Arc::new(AtomicU64::new(1));
    let ext_review_ids = Arc::new(AtomicU64::new(1));
    let session_elicitations = pending_elicitations.clone();
    let next_elicitation_id = Arc::new(AtomicU64::new(1));
    let permission_policy = spec.execution_policy;
    let permission_harness = spec.harness;
    let plan_implementation_slot = PlanImplementationSlot::default();
    let permission_implementation_slot = plan_implementation_slot.clone();
    let terminals = TerminalRegistry::new();
    let create_terminals = terminals.clone();
    let output_terminals = terminals.clone();
    let wait_terminals = terminals.clone();
    let kill_terminals = terminals.clone();
    let release_terminals = terminals.clone();
    let create_events = events.clone();
    let create_activity = spec.acp_activity.clone();
    let create_step_clock = spec.step_clock.clone();
    let output_activity = spec.acp_activity.clone();
    let wait_activity = spec.acp_activity.clone();
    let kill_activity = spec.acp_activity.clone();
    let release_activity = spec.acp_activity.clone();
    // A terminal runs where the session runs unless the agent names a
    // directory of its own.
    let session_cwd = spec.cwd.clone();
    let restart = Arc::new(Mutex::new(None));
    let restart_slot = restart.clone();
    Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _cx| {
                notification_activity.mark();
                notification_step_clock.observe(&notification.update);
                if !notification_session_updates_enabled.load(Ordering::Acquire) {
                    return Ok(());
                }
                if !session_update_is_relay_visible(
                    &notification.update,
                    &notification_live_tool_calls,
                    &notification.session_id.to_string(),
                ) {
                    return Ok(());
                }
                let update = serde_json::to_value(notification.update).map_err(|error| {
                    agent_client_protocol::Error::internal_error().data(serde_json::Value::String(
                        format!("serialize ACP session update for relay: {error}"),
                    ))
                })?;
                notification_session_update_count.fetch_add(1, Ordering::Release);
                notification_events
                    .send(RuntimeEvent::SessionUpdate { update })
                    .await
                    .map_err(|_| relay_event_channel_error())?;
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _cx| {
                permission_activity.mark();
                permission_step_clock.begin_client_work();
                if is_plan_permission(&request) {
                    let id = format!(
                        "plan-review-{}",
                        permission_review_ids.fetch_add(1, Ordering::Relaxed)
                    );
                    let value = serde_json::to_value(&request)
                        .map_err(|_| agent_client_protocol::Error::internal_error())?;
                    let review = normalized_plan_review(id.clone(), &value);
                    let approved_plan = plan_review_proposal(&review).unwrap_or_default().to_owned();
                    let implementation = permission_implementation_slot
                        .lock().expect("plan implementation lock poisoned").clone();
                    let (answer, answer_rx) = oneshot::channel();
                    permission_elicitations
                        .lock()
                        .expect("pending elicitation lock poisoned")
                        .insert(id.clone(), answer);
                    let pending = permission_elicitations.clone();
                    let events = permission_events.clone();
                    let cancellation = responder.cancellation();
                    tokio::spawn(async move {
                        if events
                            .send(RuntimeEvent::ElicitationRequested { request: review })
                            .await
                            .is_err()
                        {
                            pending
                                .lock()
                                .expect("pending elicitation lock poisoned")
                                .remove(&id);
                            if let Err(error) =
                                responder.respond_with_error(relay_event_channel_error())
                            {
                                tracing::debug!(
                                    %id,
                                    operation = "permission_request",
                                    %error,
                                    "could not report a stopped relay coordinator to ACP"
                                );
                            }
                            return;
                        }
                        let response = tokio::select! {
                            response = answer_rx => response.ok(),
                            () = cancellation.cancelled() => None,
                        };
                        pending
                            .lock()
                            .expect("pending elicitation lock poisoned")
                            .remove(&id);
                        let action = response
                            .as_ref()
                            .map_or("cancel", ElicitationResponse::action_name)
                            .to_owned();
                        if let Err(error) = events
                            .send(RuntimeEvent::ElicitationResolved {
                                elicitation_id: id.clone(),
                                action,
                            })
                            .await
                        {
                            tracing::debug!(
                                %id,
                                operation = "elicitation_resolved",
                                %error,
                                "could not report permission response to relay coordinator"
                            );
                        }
                        let response = if cancellation.is_cancelled() { None } else { response };
                        let mut handoff_completion = None;
                        let selection = response.map_or_else(
                            || Ok(PlanPermissionAnswer::Native(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled))),
                            |response| policy_plan_permission_answer(&request, response, permission_harness, permission_policy),
                        ).and_then(|selection| match selection {
                            PlanPermissionAnswer::Native(answer) => Ok(answer),
                            PlanPermissionAnswer::ContinueInBypass => {
                                let (completion, permission_sent) = oneshot::channel();
                                implementation.as_ref()
                                    .ok_or_else(|| anyhow!("Cannot resume the approved plan without an active prompt; select bypassPermissions and submit the implementation instruction."))?
                                    .send(PlanImplementation { plan: approved_plan, permission_sent })
                                    .map_err(|_| anyhow!("Plan implementation was cancelled because its prompt is no longer active."))?;
                                handoff_completion = Some(completion);
                                Ok(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled))
                            }
                        });
                        let answer = match selection {
                            Ok(answer) => answer,
                            Err(error) => {
                                if events.send(RuntimeEvent::Warning { message: format!("{error:#}") }).await.is_err() {
                                    tracing::debug!(%error, "could not report failed plan implementation");
                                }
                                RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
                            }
                        };
                        let result = responder.respond(answer);
                        if let Some(completion) = handoff_completion
                            && completion.send(result.is_ok()).is_err()
                        {
                            tracing::debug!(%id, "plan implementation stopped before the permission response was delivered");
                        }
                        if let Err(error) = result {
                            tracing::debug!(
                                %id,
                                operation = "permission_response",
                                %error,
                                "ACP permission responder was already closed"
                            );
                        }
                    });
                    return Ok(());
                }
                // A permission request that is_plan_permission() did not classify
                // reaches the deny path below. Log its raw shape so an agent whose
                // request form we do not yet recognize is diagnosable from
                // worker.log instead of only surfacing as a silent denial.
                match serde_json::to_value(&request) {
                    Ok(raw) => tracing::debug!(
                        target: "hel_acp::plan_diag",
                        operation = "unclassified_permission_request",
                        request = %raw,
                        "permission request not classified as a plan review; raw payload follows"
                    ),
                    Err(error) => tracing::debug!(
                        target: "hel_acp::plan_diag",
                        operation = "unclassified_permission_request",
                        %error,
                        "permission request not classified as a plan review and could not be serialized"
                    ),
                }
                if permission_policy.is_unconstrained() {
                    permission_events
                        .send(RuntimeEvent::Warning {
                            message: UNEXPECTED_PERMISSION_REQUEST_WARNING.to_owned(),
                        })
                        .await
                        .map_err(|_| relay_event_channel_error())?;
                }
                // Permission escalations are denied safely because Hel has no
                // per-action human approval surface. An unconstrained harness
                // must never ask; denying instead of auto-approving makes a
                // broken mode selection visible rather than masking it.
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: CreateTerminalRequest, responder, _cx| {
                create_activity.mark();
                create_step_clock.begin_client_work();
                let started_at_ms = crate::clock::epoch_millis();
                let spawn = TerminalSpawn {
                    command: request.command.clone(),
                    args: request.args.clone(),
                    // Additions, not a replacement: the child inherits the
                    // daemon environment it needs to reach the toolchain.
                    env: request
                        .env
                        .iter()
                        .map(|variable| (variable.name.clone(), variable.value.clone()))
                        .collect(),
                    cwd: request.cwd.clone().unwrap_or_else(|| session_cwd.clone()),
                    output_byte_limit: request
                        .output_byte_limit
                        .and_then(|limit| usize::try_from(limit).ok())
                        .unwrap_or(DEFAULT_TERMINAL_OUTPUT_BYTES),
                };
                let command = spawn.display_command();
                match create_terminals.create(spawn, create_events.clone()) {
                    Ok(terminal_id) => {
                        create_events
                            .send(RuntimeEvent::TerminalStarted {
                                terminal_id: terminal_id.clone(),
                                command,
                                started_at_ms,
                            })
                            .await
                            .map_err(|_| relay_event_channel_error())?;
                        responder
                            .respond(CreateTerminalResponse::new(TerminalId::from(terminal_id)))
                    }
                    Err(error) => {
                        create_events
                            .send(RuntimeEvent::Warning {
                                message: format!("a client terminal failed to start: {error:#}"),
                            })
                            .await
                            .map_err(|_| relay_event_channel_error())?;
                        responder.respond_with_error(
                            agent_client_protocol::Error::internal_error()
                                .data(serde_json::Value::String(format!("{error:#}"))),
                        )
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: TerminalOutputRequest, responder, _cx| {
                output_activity.mark();
                let terminal_id = request.terminal_id.to_string();
                let Some(snapshot) = output_terminals.output(&terminal_id) else {
                    return responder.respond_with_error(unknown_terminal_error(&terminal_id));
                };
                let mut response = TerminalOutputResponse::new(snapshot.output, snapshot.truncated);
                if let Some(exit) = &snapshot.exit {
                    response = response.exit_status(terminal_exit_status(exit));
                }
                responder.respond(response)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: WaitForTerminalExitRequest, responder, _cx| {
                wait_activity.mark();
                let terminal_id = request.terminal_id.to_string();
                let Some(exit) = wait_terminals.exit_receiver(&terminal_id) else {
                    return responder.respond_with_error(unknown_terminal_error(&terminal_id));
                };
                // Handlers run on the dispatch loop, so awaiting the child here
                // would stop every other message until it exits.
                tokio::spawn(async move {
                    let exit = crate::hel_terminal::wait_for_exit(exit).await;
                    if let Err(error) = responder.respond(WaitForTerminalExitResponse::new(
                        terminal_exit_status(&exit),
                    )) {
                        // A closed channel means the relay already stopped, so
                        // this warning has nowhere left to go.
                        tracing::debug!(
                            %terminal_id,
                            operation = "terminal_wait_response",
                            %error,
                            "ACP terminal wait responder was already closed"
                        );
                    }
                });
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: KillTerminalRequest, responder, _cx| {
                kill_activity.mark();
                let terminal_id = request.terminal_id.to_string();
                // The terminal stays valid: output and wait_for_exit still
                // answer for it until the agent releases it.
                if !kill_terminals.kill(&terminal_id) {
                    return responder.respond_with_error(unknown_terminal_error(&terminal_id));
                }
                responder.respond(KillTerminalResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: ReleaseTerminalRequest, responder, _cx| {
                release_activity.mark();
                let terminal_id = request.terminal_id.to_string();
                let Some(supervisor) = release_terminals.release(&terminal_id) else {
                    return responder.respond_with_error(unknown_terminal_error(&terminal_id));
                };
                // Reap off the dispatch loop: the supervisor still has to watch
                // the killed child exit before it reports the terminal closed.
                tokio::spawn(async move {
                    if let Err(error) = supervisor.await {
                        tracing::warn!(
                            %terminal_id,
                            operation = "terminal_release_reap",
                            %error,
                            "released terminal supervisor failed"
                        );
                    }
                });
                responder.respond(ReleaseTerminalResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        // Catch-all, registered last so the typed handlers above win. The ACP
        // crate parks an unhandled request that carries a session id instead of
        // rejecting it, so without this an agent that sends an ext request Hel
        // does not know waits for a reply that never comes, and its turn never
        // ends. Hel answers every incoming request, always.
        .on_receive_request(
            async move |request: agent_client_protocol::UntypedMessage, responder, _cx| {
                ext_activity.mark();
                ext_step_clock.begin_client_work();
                let method = request.method().to_owned();
                if method == "elicitation/create" {
                    let id = format!(
                        "elicitation-{}",
                        next_elicitation_id.fetch_add(1, Ordering::Relaxed)
                    );
                    let request = match ElicitationRequest::from_acp_params(
                        id.clone(),
                        request.params().clone(),
                    ) {
                        Ok(request) => request,
                        Err(error) => {
                            return responder.respond_with_error(
                                agent_client_protocol::Error::invalid_params().data(
                                    serde_json::Value::String(format!(
                                        "invalid ACP form elicitation: {error:#}"
                                    )),
                                ),
                            );
                        }
                    };
                    let (answer, answer_rx) = oneshot::channel();
                    handler_elicitations
                        .lock()
                        .expect("pending elicitation lock poisoned")
                        .insert(id.clone(), answer);
                    let pending = handler_elicitations.clone();
                    let events = elicitation_events.clone();
                    let cancellation = responder.cancellation();
                    tokio::spawn(async move {
                        if events
                            .send(RuntimeEvent::ElicitationRequested { request })
                            .await
                            .is_err()
                        {
                            pending
                                .lock()
                                .expect("pending elicitation lock poisoned")
                                .remove(&id);
                            if let Err(error) =
                                responder.respond_with_error(relay_event_channel_error())
                            {
                                tracing::debug!(
                                    %id,
                                    operation = "elicitation_request",
                                    %error,
                                    "could not report a stopped relay coordinator to ACP"
                                );
                            }
                            return;
                        }
                        let response = tokio::select! {
                            response = answer_rx => response.ok(),
                            () = cancellation.cancelled() => None,
                        };
                        pending
                            .lock()
                            .expect("pending elicitation lock poisoned")
                            .remove(&id);
                        let action = response
                            .as_ref()
                            .map_or("cancel", ElicitationResponse::action_name)
                            .to_owned();
                        if let Err(error) = events
                            .send(RuntimeEvent::ElicitationResolved {
                                elicitation_id: id.clone(),
                                action,
                            })
                            .await
                        {
                            tracing::debug!(
                                %id,
                                operation = "elicitation_resolved",
                                %error,
                                "could not report elicitation response to relay coordinator"
                            );
                        }
                        match response {
                            Some(response) => match serde_json::to_value(response) {
                                Ok(response) => {
                                    if let Err(error) = responder.respond(response) {
                                        tracing::debug!(
                                            %id,
                                            operation = "elicitation_response",
                                            %error,
                                            "ACP elicitation responder was already closed"
                                        );
                                    }
                                }
                                Err(error) => {
                                    if let Err(error) = responder.respond_with_error(
                                        agent_client_protocol::Error::internal_error().data(
                                            serde_json::Value::String(format!(
                                                "serialize elicitation response: {error}"
                                            )),
                                        ),
                                    ) {
                                        tracing::debug!(
                                            %id,
                                            operation = "elicitation_response",
                                            %error,
                                            "ACP elicitation error responder was already closed"
                                        );
                                    }
                                }
                            },
                            None => {
                                if let Err(error) = responder.respond_with_error(
                                    agent_client_protocol::Error::request_cancelled(),
                                ) {
                                    tracing::debug!(
                                        %id,
                                        operation = "elicitation_cancel",
                                        %error,
                                        "ACP cancellation responder was already closed"
                                    );
                                }
                            }
                        }
                    });
                    return Ok(());
                }
                if grok::handles_exit_plan_mode(ext_harness, &method) {
                    let id = grok::plan_review_id(ext_review_ids.fetch_add(1, Ordering::Relaxed));
                    let review = normalized_plan_review(id.clone(), request.params());
                    let (answer, answer_rx) = oneshot::channel();
                    handler_elicitations
                        .lock()
                        .expect("pending elicitation lock poisoned")
                        .insert(id.clone(), answer);
                    let pending = handler_elicitations.clone();
                    let events = ext_events.clone();
                    let cancellation = responder.cancellation();
                    tokio::spawn(async move {
                        if events
                            .send(RuntimeEvent::ElicitationRequested { request: review })
                            .await
                            .is_err()
                        {
                            pending
                                .lock()
                                .expect("pending elicitation lock poisoned")
                                .remove(&id);
                            if let Err(error) =
                                responder.respond_with_error(relay_event_channel_error())
                            {
                                tracing::debug!(
                                    %id,
                                    operation = "plan_review_request",
                                    %error,
                                    "could not report a stopped relay coordinator to ACP"
                                );
                            }
                            return;
                        }
                        let response = tokio::select! {
                            response = answer_rx => response.ok(),
                            () = cancellation.cancelled() => None,
                        };
                        pending
                            .lock()
                            .expect("pending elicitation lock poisoned")
                            .remove(&id);
                        let action = response
                            .as_ref()
                            .map_or("cancel", ElicitationResponse::action_name)
                            .to_owned();
                        if let Err(error) = events
                            .send(RuntimeEvent::ElicitationResolved {
                                elicitation_id: id.clone(),
                                action,
                            })
                            .await
                        {
                            tracing::debug!(
                                %id,
                                operation = "plan_review_resolved",
                                %error,
                                "could not report plan review response to relay coordinator"
                            );
                        }
                        if let Err(error) = responder.respond(response.map_or_else(
                            || serde_json::json!({ "outcome": "cancelled" }),
                            grok::plan_response,
                        )) {
                            tracing::debug!(
                                %id,
                                operation = "plan_review_response",
                                %error,
                                "ACP plan review responder was already closed"
                            );
                        }
                    });
                    return Ok(());
                }
                ext_events
                    .send(RuntimeEvent::Warning {
                        message: unsupported_client_request_report(&method),
                    })
                    .await
                    .map_err(|_| relay_event_channel_error())?;
                responder.respond_with_error(
                    agent_client_protocol::Error::method_not_found()
                        .data(serde_json::Value::String(method)),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(transport, |connection: ConnectionTo<Agent>| async move {
            match drive_connection(
                connection,
                &spec,
                requests,
                &events,
                terminals,
                session_elicitations,
                plan_implementation_slot,
                opened,
                session_update_count,
                session_updates_enabled,
                replacing_previous_bridge,
            )
            .await
            {
                Ok(native_session_id) => {
                    *restart_slot.lock().expect("ACP restart slot lock poisoned") =
                        native_session_id;
                    Ok(())
                }
                Err(error) => Err(agent_client_protocol::Error::internal_error()
                    .data(serde_json::Value::String(format!("{error:#}")))),
            }
        })
        .await
        .map_err(|error| {
            anyhow!(
                "ACP protocol failed: {error}; bridge stdout must contain only JSON-RPC frames \
                 and login-shell startup must be silent"
            )
        })?;
    Ok(restart
        .lock()
        .expect("ACP restart slot lock poisoned")
        .take())
}

/// Stop reason reported for a turn the bridge rejected instead of finishing.
const PROMPT_ERROR_STOP_REASON: &str = "error";

/// Marker Hel adds to the warning for a prompt the bridge failed with ACP's
/// `auth_required`. The wire message is a bare "Authentication required", too
/// generic for `hel_credentials` to match on text alone, so the error code —
/// not the bridge's wording — decides whether the credential heuristic fires.
pub const PROMPT_AUTH_REQUIRED_MARKER: &str = "ACP auth_required";

/// Marker on a successful ACP response that carried no session updates. Some
/// bridges use this shape when their underlying turn failed, so completing it
/// silently would leave a user line with no answer or explanation.
pub const PROMPT_EMPTY_RESPONSE_MARKER: &str = "ACP prompt returned no session updates";

fn prompt_failure_warning(error: &agent_client_protocol::Error) -> String {
    if error.code == agent_client_protocol::ErrorCode::AuthRequired {
        format!("prompt failed ({PROMPT_AUTH_REQUIRED_MARKER}): {error}")
    } else {
        format!("prompt failed: {error}")
    }
}

fn prompt_returned_without_updates(
    stop_reason: &StopReason,
    updates_before: u64,
    updates_after: u64,
) -> bool {
    *stop_reason != StopReason::Cancelled && updates_before == updates_after
}

#[allow(clippy::too_many_arguments)]
async fn drive_connection(
    connection: ConnectionTo<Agent>,
    spec: &LaunchSpec,
    requests: &mut mpsc::Receiver<CommandRequest>,
    events: &mpsc::Sender<RuntimeEvent>,
    terminals: TerminalRegistry,
    pending_elicitations: PendingElicitations,
    plan_implementation_slot: PlanImplementationSlot,
    opened: Arc<Mutex<Option<OpenedSession>>>,
    session_update_count: Arc<AtomicU64>,
    session_updates_enabled: Arc<AtomicBool>,
    replacing_previous_bridge: bool,
) -> Result<Option<String>> {
    // Terminals belong to the connection. However the session ends — closed,
    // failed, or with its command channel dropped — their process groups must
    // not outlive it.
    let result = serve_session(
        &connection,
        spec,
        requests,
        events,
        &terminals,
        &pending_elicitations,
        &plan_implementation_slot,
        opened,
        &session_update_count,
        &session_updates_enabled,
        replacing_previous_bridge,
    )
    .await;
    pending_elicitations
        .lock()
        .expect("pending elicitation lock poisoned")
        .clear();
    terminals.shutdown(events).await;
    result
}

async fn apply_cancel(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    cancel_id: String,
    events: &mpsc::Sender<RuntimeEvent>,
    terminals: &TerminalRegistry,
) -> Result<()> {
    terminals.kill_live();
    match connection.send_notification(CancelNotification::new(session_id.clone())) {
        Ok(()) => {
            emit_runtime_event(
                events,
                RuntimeEvent::CancelApplied {
                    request_id: cancel_id,
                },
            )
            .await
        }
        Err(error) => {
            emit_runtime_event(
                events,
                RuntimeEvent::CommandRejected {
                    request_id: cancel_id,
                    message: format!("cancel ACP prompt: {error}"),
                },
            )
            .await
        }
    }
}

const SESSION_STEERING_METHOD: &str = "_session/steering";

fn steering_supported_from_meta(meta: Option<&agent_client_protocol::schema::v1::Meta>) -> bool {
    meta.and_then(|meta| meta.get("steering"))
        .and_then(|steering| steering.get("supported"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

struct PendingSteer {
    request_id: String,
    queued_command_id: String,
    response: Pin<
        Box<
            dyn Future<
                    Output = std::result::Result<serde_json::Value, agent_client_protocol::Error>,
                > + Send,
        >,
    >,
}

fn start_steer(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    request_id: String,
    steering_prompt: ClaimedSteeringPrompt,
) -> PendingSteer {
    let request = UntypedMessage {
        method: SESSION_STEERING_METHOD.to_owned(),
        params: serde_json::json!({
            "sessionId": session_id,
            "prompt": steering_prompt.prompt,
            "_meta": { "steering": { "idleBehavior": "promptRequired" } },
        }),
    };
    PendingSteer {
        request_id,
        queued_command_id: steering_prompt.queued_command_id,
        response: Box::pin(connection.send_request(request).block_task()),
    }
}

async fn settle_steer(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    events: &mpsc::Sender<RuntimeEvent>,
    terminals: &TerminalRegistry,
    pending: PendingSteer,
    outcome: std::result::Result<serde_json::Value, agent_client_protocol::Error>,
    turn_running: bool,
) -> Result<bool> {
    match outcome
        .as_ref()
        .ok()
        .and_then(|value| value.get("outcome"))
        .and_then(serde_json::Value::as_str)
    {
        Some("injected") => {
            emit_runtime_event(
                events,
                RuntimeEvent::SteerApplied {
                    request_id: pending.request_id,
                    queued_command_id: pending.queued_command_id,
                },
            )
            .await?;
            Ok(false)
        }
        outcome => {
            let detached_turn = outcome == Some("startedNewTurn");
            if turn_running || detached_turn {
                apply_cancel(
                    connection,
                    session_id,
                    pending.request_id,
                    events,
                    terminals,
                )
                .await?;
                Ok(true)
            } else {
                emit_runtime_event(
                    events,
                    RuntimeEvent::CancelApplied {
                        request_id: pending.request_id,
                    },
                )
                .await?;
                Ok(false)
            }
        }
    }
}

/// Discard requests left in the channel by the bridge that just restarted. See
/// the call site in [`serve_session`] for why nothing is reported back.
fn drain_requests_from_the_previous_bridge(requests: &mut mpsc::Receiver<CommandRequest>) {
    while let Ok(request) = requests.try_recv() {
        let (variant, request_id) = match request {
            CommandRequest::Prompt { request_id, .. } => ("Prompt", Some(request_id)),
            CommandRequest::SetConfig { request_id, .. } => ("SetConfig", Some(request_id)),
            CommandRequest::SetSessionMode { request_id, .. } => {
                ("SetSessionMode", Some(request_id))
            }
            CommandRequest::Cancel { request_id, .. } => ("Cancel", Some(request_id)),
            CommandRequest::Close { request_id } => ("Close", Some(request_id)),
            CommandRequest::ResolveElicitation { .. } => ("ResolveElicitation", None),
        };
        tracing::debug!(
            operation = "acp_bridge_restart",
            variant,
            request_id = request_id.as_deref().unwrap_or("-"),
            "dropping a request queued for the previous ACP bridge"
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve_session(
    connection: &ConnectionTo<Agent>,
    spec: &LaunchSpec,
    requests: &mut mpsc::Receiver<CommandRequest>,
    events: &mpsc::Sender<RuntimeEvent>,
    terminals: &TerminalRegistry,
    pending_elicitations: &PendingElicitations,
    plan_implementation_slot: &PlanImplementationSlot,
    opened: Arc<Mutex<Option<OpenedSession>>>,
    session_update_count: &AtomicU64,
    session_updates_enabled: &AtomicBool,
    replacing_previous_bridge: bool,
) -> Result<Option<String>> {
    let mut meta = serde_json::Map::new();
    meta.insert("terminal_output".into(), serde_json::Value::Bool(true));
    // Kimi routes every shell call through the client's terminal surface and
    // has no local fallback, so this capability is what makes Bash work.
    let capabilities = ClientCapabilities::new()
        .terminal(true)
        .elicitation(ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()))
        .meta(meta);
    let initialized = connection
        .send_request(
            InitializeRequest::new(ProtocolVersion::V1)
                .client_info(
                    Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
                        .title("Mjolnir"),
                )
                .client_capabilities(capabilities),
        )
        .block_task()
        .await;
    spec.acp_activity.mark();
    let initialized = initialized.context("initialize ACP bridge")?;
    if initialized.protocol_version != ProtocolVersion::V1 {
        bail!(
            "ACP bridge negotiated unsupported protocol {:?}",
            initialized.protocol_version
        );
    }
    let steering_supported = steering_supported_from_meta(initialized.meta.as_ref());
    // Grok Build publishes its catalogue here rather than as `configOptions`.
    let mut grok_models = (spec.harness == HarnessKind::Grok)
        .then(|| grok::model_state(initialized.meta.as_ref()))
        .flatten();
    emit_runtime_event(
        events,
        RuntimeEvent::Connected {
            agent_name: initialized
                .agent_info
                .as_ref()
                .map(|info| info.name.clone()),
            agent_version: initialized
                .agent_info
                .as_ref()
                .map(|info| info.version.clone()),
            protocol_version: Some(initialized.protocol_version),
            capabilities: Some(Box::new(initialized.agent_capabilities.clone())),
            agent_info: initialized.agent_info.clone(),
        },
    )
    .await?;

    let loaded_session = if let Some(existing) = &spec.resume_session {
        let loaded = connection
            .send_request(load_session_request(
                spec,
                SessionId::from(existing.clone()),
            ))
            .block_task()
            .await;
        spec.acp_activity.mark();
        let loaded = loaded.with_context(|| format!("load ACP session {existing}"))?;
        if let Some(state) = grok_models.as_mut()
            && let Some(fresh) = grok::model_state(loaded.meta.as_ref())
        {
            *state = fresh;
        }
        // The response is the boundary between provider replay and future
        // live updates for this connection.
        session_updates_enabled.store(true, Ordering::Release);
        Some((
            SessionId::from(existing.clone()),
            loaded.config_options,
            loaded.modes,
        ))
    } else {
        None
    };
    let (session_id, config_options, modes, resumed) =
        if let Some((id, options, modes)) = loaded_session {
            (id, options, modes, true)
        } else {
            let created = connection
                .send_request(new_session_request(spec, true))
                .block_task()
                .await;
            spec.acp_activity.mark();
            let created = created.context("create ACP session")?;
            // A session may open on a different model than the agent-wide
            // default, so a fresher catalogue on the session wins.
            if let Some(state) = grok_models.as_mut()
                && let Some(fresh) = grok::model_state(created.meta.as_ref())
            {
                *state = fresh;
            }
            (
                created.session_id,
                created.config_options,
                created.modes,
                false,
            )
        };

    // Launch flags and environment are applied before the bridge starts. ACP
    // modes are selected after the session exists, before any prompt can run.
    let enforcement = spec.harness.execution_enforcement(spec.execution_policy);
    let mut config_options = config_options.unwrap_or_default();
    let mut modes = modes;
    if let Some(desired_mode) = enforcement.and_then(ExecutionEnforcement::acp_mode) {
        enforce_execution_mode(
            connection,
            &session_id,
            desired_mode,
            &mut config_options,
            &mut modes,
        )
        .await?;
    }
    // Grok Build publishes model selection through its legacy catalogue. Keep
    // any standard selectors it also returns while projecting model/effort
    // into the shape the rest of Hel reads.
    if let Some(state) = &grok_models {
        grok::merge_config_options(&mut config_options, state);
    }
    let accepted = spec
        .accepted_config
        .lock()
        .map_err(|_| anyhow!("accepted session configuration lock was poisoned"))?
        .clone();
    // Model selection can replace the effort catalogue. Both must be
    // restored before SessionConfigured releases queued prompts.
    for (key, value) in [("model", accepted.model), ("effort", accepted.effort)] {
        let Some(value) = value else { continue };
        apply_session_selector(
            connection,
            &session_id,
            &mut config_options,
            &mut grok_models,
            key,
            &value,
        )
        .await
        .with_context(|| format!("restore this session's accepted {key} {value:?}"))?;
    }
    // Startup failures must retain their cause rather than being classified
    // as a dead running bridge and retried with the same invalid settings.
    *opened.lock().expect("opened session lock poisoned") = Some(OpenedSession {
        native_session_id: session_id.to_string(),
        started_at: tokio::time::Instant::now(),
    });
    // Drop anything the worker queued for the bridge this one replaced. The
    // worker dispatches only while it believes the session is configured; it
    // clears that flag on `HarnessRestarting` and sets it again only after this
    // bridge's `SessionConfigured`, which has not been sent yet. So every
    // request still in the channel was dispatched before the worker saw the
    // restart and is already in the set the worker interrupted. Emitting a
    // runtime event for one would interrupt it twice and fail the coordinator's
    // `require_in_flight`; delivering it would run it untracked on the fresh
    // session. A first start drains nothing: out-of-band senders such as
    // compaction are not gated on the session being configured, so a request
    // that arrives while the very first bridge is still handshaking is a live
    // request for this session, not a leftover.
    if replacing_previous_bridge {
        drain_requests_from_the_previous_bridge(requests);
    }

    emit_runtime_event(
        events,
        RuntimeEvent::SessionStarted {
            native_session_id: session_id.to_string(),
            resumed,
            execution_mode: enforcement.map(|enforcement| enforcement.label().to_owned()),
        },
    )
    .await?;
    emit_runtime_event(
        events,
        RuntimeEvent::SessionConfigured {
            config_options: config_options.clone(),
        },
    )
    .await?;
    emit_runtime_event(
        events,
        RuntimeEvent::SessionModesConfigured {
            modes: modes.clone(),
        },
    )
    .await?;

    while let Some(request) = requests.recv().await {
        match request {
            CommandRequest::Prompt { request_id, prompt } => {
                if prompt.is_empty() {
                    emit_runtime_event(
                        events,
                        RuntimeEvent::CommandRejected {
                            request_id,
                            message: "ACP prompt has no content blocks".into(),
                        },
                    )
                    .await?;
                    continue;
                }
                let mut updates_before = session_update_count.load(Ordering::Acquire);
                spec.step_clock.begin_turn();
                let mut prompt: ActivePrompt = Box::pin(
                    connection
                        .send_request(PromptRequest::new(session_id.clone(), prompt))
                        .block_task(),
                );
                let (implementation_tx, mut implementation_rx) = mpsc::unbounded_channel();
                *plan_implementation_slot
                    .lock()
                    .expect("plan implementation lock poisoned") = Some(implementation_tx);
                let _active_implementation =
                    ActivePlanImplementation(plan_implementation_slot.clone());
                let mut approved_plan = None;
                let mut implementation_deadline = None;
                let mut mode_restoration: Option<PlanModeRestoration<'_>> = None;
                let mut prompt_running = true;
                let mut cancel_deadline = None;
                let mut pending_steer: Option<PendingSteer> = None;
                loop {
                    tokio::select! {
                        biased;
                        Some(plan) = implementation_rx.recv(), if cancel_deadline.is_none() && approved_plan.is_none() && mode_restoration.is_none() => {
                            approved_plan = Some(plan);
                            implementation_deadline = Some(tokio::time::Instant::now() + CANCEL_ACK_TIMEOUT);
                            emit_runtime_event(events, RuntimeEvent::Warning {
                                message: "Plan approved; waiting for Claude to finish planning before restoring bypassPermissions.".into(),
                            }).await?;
                        }
                        response = &mut prompt, if prompt_running => {
                            spec.acp_activity.mark();
                            spec.step_clock.end_turn();
                            if approved_plan.is_some() && cancel_deadline.is_none() {
                                if matches!(&response, Ok(response) if matches!(response.stop_reason, StopReason::EndTurn | StopReason::Cancelled)) {
                                    prompt_running = false;
                                    let implementation = approved_plan.take().expect("approved plan is present");
                                    mode_restoration = Some(Box::pin(restore_plan_execution_mode(connection, session_id.clone(), RestoredPlanMode {
                                        config_options: config_options.clone(), modes: modes.clone(), plan: implementation.plan,
                                    }, implementation.permission_sent)));
                                    continue;
                                }
                                emit_runtime_event(events, RuntimeEvent::Warning {
                                    message: "Plan implementation stopped because Claude did not finish the planning turn successfully.".into(),
                                }).await?;
                            }
                            if let Some(mut pending) = pending_steer.take() {
                                match tokio::time::timeout(
                                    Duration::from_secs(2),
                                    pending.response.as_mut(),
                                )
                                .await
                                {
                                    Ok(outcome) => {
                                        settle_steer(
                                            connection,
                                            &session_id,
                                            events,
                                            terminals,
                                            pending,
                                            outcome,
                                            false,
                                        )
                                        .await?;
                                    }
                                    Err(_) => {
                                        emit_runtime_event(
                                            events,
                                            RuntimeEvent::CancelApplied {
                                                request_id: pending.request_id,
                                            },
                                        )
                                        .await?;
                                    }
                                }
                            }
                            // A rejected prompt fails the turn, not the worker: the
                            // bridge can still serve later prompts. A JSON-RPC
                            // error stays on this connection; a dead transport
                            // is recovered by `run_bridge` via child exit or a
                            // protocol error after the session is open.
                            let stop_reason = match response {
                                Ok(response) => {
                                    if prompt_returned_without_updates(
                                        &response.stop_reason,
                                        updates_before,
                                        session_update_count.load(Ordering::Acquire),
                                    ) {
                                        emit_runtime_event(
                                            events,
                                            RuntimeEvent::Warning {
                                                message: PROMPT_EMPTY_RESPONSE_MARKER.to_owned(),
                                            },
                                        )
                                        .await?;
                                    }
                                    format!("{:?}", response.stop_reason)
                                }
                                Err(error) => {
                                    emit_runtime_event(
                                        events,
                                        RuntimeEvent::Warning {
                                            message: prompt_failure_warning(&error),
                                        },
                                    )
                                    .await?;
                                    PROMPT_ERROR_STOP_REASON.to_owned()
                                }
                            };
                            emit_runtime_event(
                                events,
                                RuntimeEvent::PromptFinished {
                                    request_id,
                                    stop_reason,
                                },
                            )
                            .await?;
                            // An acknowledged cancel leaves the bridge in
                            // place; the next prompt goes to the same session.
                            break;
                        }
                        _ = async {
                            tokio::time::sleep_until(implementation_deadline.expect("implementation deadline branch is guarded")).await;
                        }, if implementation_deadline.is_some() => {
                            let message = "Plan implementation timed out while finishing planning or restoring bypassPermissions; restarting the harness without submitting the continuation.";
                            emit_runtime_event(events, RuntimeEvent::Warning { message: message.into() }).await?;
                            emit_runtime_event(events, RuntimeEvent::CommandInterrupted { request_id, message: message.into() }).await?;
                            return Ok(Some(session_id.to_string()));
                        }
                        _ = async {
                            tokio::time::sleep_until(
                                cancel_deadline.expect("cancel deadline branch is guarded"),
                            )
                            .await;
                        }, if cancel_deadline.is_some() => {
                            emit_runtime_event(
                                events,
                                RuntimeEvent::Warning {
                                    message: CANCEL_UNACKED_WARNING.to_owned(),
                                },
                            )
                            .await?;
                            emit_runtime_event(
                                events,
                                RuntimeEvent::CommandInterrupted {
                                    request_id,
                                    message: CANCEL_UNACKED_WARNING.to_owned(),
                                },
                            )
                            .await?;
                            return Ok(Some(session_id.to_string()));
                        }
                        steer_outcome = async {
                            pending_steer
                                .as_mut()
                                .expect("steering branch is guarded")
                                .response
                                .as_mut()
                                .await
                        }, if pending_steer.is_some() => {
                            let pending = pending_steer
                                .take()
                                .expect("steering branch is guarded");
                            if settle_steer(
                                connection,
                                &session_id,
                                events,
                                terminals,
                                pending,
                                steer_outcome,
                                true,
                            )
                            .await?
                                && cancel_deadline.is_none()
                            {
                                cancel_deadline =
                                    Some(tokio::time::Instant::now() + CANCEL_ACK_TIMEOUT);
                            }
                        }
                        command = requests.recv() => match command {
                            Some(CommandRequest::Cancel {
                                request_id: cancel_id,
                                steering_prompt,
                            }) => {
                                implementation_rx.close();
                                approved_plan = None;
                                implementation_deadline = None;
                                if !prompt_running {
                                    apply_cancel(connection, &session_id, cancel_id, events, terminals).await?;
                                    emit_runtime_event(events, RuntimeEvent::PromptFinished {
                                        request_id, stop_reason: "Cancelled".into(),
                                    }).await?;
                                    break;
                                }
                                if steering_supported
                                    && pending_steer.is_none()
                                    && cancel_deadline.is_none()
                                    && let Some(steering_prompt) = steering_prompt
                                {
                                    pending_steer = Some(start_steer(
                                        connection,
                                        &session_id,
                                        cancel_id,
                                        steering_prompt,
                                    ));
                                } else {
                                    apply_cancel(
                                        connection,
                                        &session_id,
                                        cancel_id,
                                        events,
                                        terminals,
                                    )
                                    .await?;
                                    if cancel_deadline.is_none() {
                                        cancel_deadline = Some(
                                            tokio::time::Instant::now() + CANCEL_ACK_TIMEOUT,
                                        );
                                    }
                                }
                            }
                            Some(CommandRequest::Close { request_id: close_id }) => {
                                if let Err(error) = connection.send_notification(CancelNotification::new(session_id.clone())) {
                                    emit_runtime_event(
                                        events,
                                        RuntimeEvent::Warning {
                                            message: format!("cancel ACP prompt before close: {error}"),
                                        },
                                    )
                                    .await?;
                                }
                                emit_runtime_event(
                                    events,
                                    RuntimeEvent::CommandInterrupted {
                                        request_id: request_id.clone(),
                                        message: "prompt interrupted because the session was closed".into(),
                                    },
                                )
                                .await?;
                                match connection
                                    .send_request(CloseSessionRequest::new(session_id.clone()))
                                    .block_task()
                                    .await
                                {
                                    Ok(_) => {
                                        emit_runtime_event(
                                            events,
                                            RuntimeEvent::CloseApplied {
                                                request_id: close_id,
                                            },
                                        )
                                        .await?;
                                    }
                                    Err(error) => {
                                        emit_runtime_event(
                                            events,
                                            RuntimeEvent::CommandRejected {
                                                request_id: close_id,
                                                message: format!("close ACP session: {error}"),
                                            },
                                        )
                                        .await?;
                                    }
                                }
                                return Ok(None);
                            }
                            None => {
                                let cancellation = connection
                                    .send_notification(CancelNotification::new(session_id.clone()));
                                emit_runtime_event(
                                    events,
                                    RuntimeEvent::CommandInterrupted {
                                        request_id: request_id.clone(),
                                        message: "ACP command channel closed while the prompt was running".into(),
                                    },
                                )
                                .await?;
                                cancellation.context("cancel ACP prompt during runtime shutdown")?;
                                return Ok(None);
                            }
                            Some(CommandRequest::Prompt { request_id, .. }) => {
                                emit_runtime_event(
                                    events,
                                    RuntimeEvent::CommandRejected {
                                        request_id,
                                        message: "a prompt is already running".into(),
                                    },
                                )
                                .await?;
                            }
                            Some(CommandRequest::SetConfig { request_id, .. }) => {
                                emit_runtime_event(
                                    events,
                                    RuntimeEvent::CommandRejected {
                                        request_id,
                                        message: "configuration can only be changed while the agent is idle".into(),
                                    },
                                )
                                .await?;
                            }
                            Some(CommandRequest::SetSessionMode { request_id, .. }) => {
                                emit_runtime_event(
                                    events,
                                    RuntimeEvent::CommandRejected {
                                        request_id,
                                        message: "the session mode can only be changed while the agent is idle".into(),
                                    },
                                )
                                .await?;
                            }
                            Some(CommandRequest::ResolveElicitation {
                                elicitation_id,
                                response,
                                resolved,
                            }) => {
                                if resolved
                                    .send(resolve_pending_elicitation(
                                        pending_elicitations,
                                        &elicitation_id,
                                        response,
                                    ))
                                    .is_err()
                                {
                                    tracing::debug!(
                                        session_id = %session_id,
                                        operation = "resolve_elicitation",
                                        %elicitation_id,
                                        "elicitation resolution receiver was already closed"
                                    );
                                }
                            }
                        },
                        restored = async {
                            mode_restoration.as_mut().expect("mode restoration branch is guarded").await
                        }, if mode_restoration.is_some() && requests.is_empty() => {
                            mode_restoration = None;
                            implementation_deadline = None;
                            match restored {
                                Ok(state) => {
                                    config_options = state.config_options;
                                    modes = state.modes;
                                    emit_runtime_event(events, RuntimeEvent::SessionConfigured { config_options: config_options.clone() }).await?;
                                    emit_runtime_event(events, RuntimeEvent::SessionModesConfigured { modes: modes.clone() }).await?;
                                    let plan = state.plan;
                                    let continuation = format!("The user approved the following plan. Implement it now; the preceding permission cancellation was mj's mode-transition handling.\n\n{plan}");
                                    updates_before = session_update_count.load(Ordering::Acquire);
                                    spec.step_clock.begin_turn();
                                    prompt = Box::pin(connection.send_request(PromptRequest::new(session_id.clone(), vec![ContentBlock::Text(TextContent::new(continuation))])).block_task());
                                    prompt_running = true;
                                }
                                Err(error) => {
                                    emit_runtime_event(events, RuntimeEvent::Warning { message: format!("Plan implementation stopped: could not restore bypassPermissions: {error:#}") }).await?;
                                    emit_runtime_event(events, RuntimeEvent::PromptFinished { request_id, stop_reason: PROMPT_ERROR_STOP_REASON.into() }).await?;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            CommandRequest::SetConfig {
                request_id,
                key,
                value,
            } => {
                let grok_model_change = grok_models.is_some() && grok::handles_config_key(&key);
                let applied = apply_session_selector(
                    connection,
                    &session_id,
                    &mut config_options,
                    &mut grok_models,
                    &key,
                    &value,
                )
                .await;
                match applied {
                    Ok(()) => {
                        spec.accepted_config
                            .lock()
                            .map_err(|_| {
                                anyhow!("accepted session configuration lock was poisoned")
                            })?
                            .remember(&key, &value, &config_options);
                        emit_runtime_event(
                            events,
                            RuntimeEvent::ConfigApplied {
                                request_id,
                                key,
                                value,
                                config_options: config_options.clone(),
                            },
                        )
                        .await?;
                    }
                    Err(error) => {
                        if grok_model_change && grok::response_was_lost(&error) {
                            return Err(error.context(
                                "Grok model change response was lost; reload the session to reconcile its model state",
                            ));
                        }
                        emit_runtime_event(
                            events,
                            RuntimeEvent::CommandRejected {
                                request_id,
                                message: format!("{error:#}"),
                            },
                        )
                        .await?;
                    }
                }
            }
            CommandRequest::SetSessionMode {
                request_id,
                mode_id,
            } => {
                let advertised = modes.as_ref().is_some_and(|state| {
                    state
                        .available_modes
                        .iter()
                        .any(|mode| mode.id.to_string() == mode_id)
                });
                let grok_plan_fallback =
                    grok::permits_unadvertised_plan_mode(spec.harness, &mode_id);
                let applied = if advertised || grok_plan_fallback {
                    connection
                        .send_request(SetSessionModeRequest::new(
                            session_id.clone(),
                            mode_id.clone(),
                        ))
                        .block_task()
                        .await
                        .map(|_| ())
                        .with_context(|| format!("set session mode to {mode_id}"))
                } else {
                    Err(anyhow!("{mode_id:?} is not an available session mode"))
                };
                match applied {
                    Ok(()) => {
                        if let Some(state) = modes.as_mut() {
                            state.current_mode_id = mode_id.clone().into();
                        }
                        emit_runtime_event(
                            events,
                            RuntimeEvent::SessionModeApplied {
                                request_id,
                                mode_id,
                                config_options: config_options.clone(),
                                modes: modes.clone(),
                            },
                        )
                        .await?;
                    }
                    Err(error) => {
                        emit_runtime_event(
                            events,
                            RuntimeEvent::CommandRejected {
                                request_id,
                                message: format!("{error:#}"),
                            },
                        )
                        .await?;
                    }
                }
            }
            CommandRequest::Cancel { request_id, .. } => {
                apply_cancel(connection, &session_id, request_id, events, terminals).await?;
            }
            CommandRequest::ResolveElicitation {
                elicitation_id,
                response,
                resolved,
            } => {
                if resolved
                    .send(resolve_pending_elicitation(
                        pending_elicitations,
                        &elicitation_id,
                        response,
                    ))
                    .is_err()
                {
                    tracing::debug!(
                        session_id = %session_id,
                        operation = "resolve_elicitation",
                        %elicitation_id,
                        "elicitation resolution receiver was already closed"
                    );
                }
            }
            CommandRequest::Close { request_id } => {
                match connection
                    .send_request(CloseSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await
                {
                    Ok(_) => {
                        emit_runtime_event(events, RuntimeEvent::CloseApplied { request_id })
                            .await?;
                    }
                    Err(error) => {
                        emit_runtime_event(
                            events,
                            RuntimeEvent::CommandRejected {
                                request_id,
                                message: format!("close ACP session: {error}"),
                            },
                        )
                        .await?;
                    }
                }
                break;
            }
        }
    }
    Ok(None)
}

fn resolve_pending_elicitation(
    pending: &PendingElicitations,
    elicitation_id: &str,
    response: ElicitationResponse,
) -> std::result::Result<(), String> {
    let Some(answer) = pending
        .lock()
        .expect("pending elicitation lock poisoned")
        .remove(elicitation_id)
    else {
        return Err(format!(
            "elicitation {elicitation_id:?} is no longer pending"
        ));
    };
    answer
        .send(response)
        .map_err(|_| format!("elicitation {elicitation_id:?} was cancelled before it was answered"))
}

async fn apply_session_selector(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    options: &mut Vec<SessionConfigOption>,
    grok_models: &mut Option<grok::GrokModelState>,
    key: &str,
    value: &str,
) -> Result<()> {
    match grok_models.as_mut() {
        Some(state) if grok::handles_config_key(key) => {
            grok::apply_model_change(connection, session_id, state, key, value)
                .await
                .inspect(|()| grok::merge_config_options(options, state))
        }
        _ => set_session_config(connection, session_id, options, key, value).await,
    }
}

async fn set_session_config(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    options: &mut Vec<SessionConfigOption>,
    key: &str,
    value: &str,
) -> Result<()> {
    let option = find_session_config_option(options, key)
        .with_context(|| format!("ACP bridge does not expose a {key} selector"))?;
    ensure!(
        select_contains(&option.kind, value),
        "{value:?} is not an available {key} value"
    );
    let response = connection
        .send_request(SetSessionConfigOptionRequest::new(
            session_id.clone(),
            option.id.clone(),
            SessionConfigValueId::new(value.to_owned()),
        ))
        .block_task()
        .await
        .with_context(|| format!("set session {key} to {value}"))?;
    *options = response.config_options;
    Ok(())
}

/// One selectable value of a session configuration option, flattened out of
/// the harness's ACP select shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfigChoice {
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

/// Every value the harness currently advertises for `key`, in advertised
/// order and with option groups flattened.
///
/// Empty when the harness advertises no such option or exposes it as
/// something other than a select, which callers read as "not configurable".
#[must_use]
/// What one live session's ACP surface offers, for callers outside the chat.
///
/// The phone server and the terminal must agree on which harness drives plan
/// mode through a session mode and which drives it through a configuration
/// key, on whether fast mode is available, and on which values a setting
/// accepts. Those are facts about the harness rather than about the client, so
/// they are answered here once instead of being decided again in each surface.
pub struct AcpSessionFacts(crate::hel_acp::surface::AcpSessionSurface);

impl AcpSessionFacts {
    /// Read the facts out of one relay operational snapshot.
    pub fn from_operational(
        harness_kind: HarnessKind,
        configuration: &std::collections::BTreeMap<String, String>,
        config_options: &[SessionConfigOption],
        modes: Option<&agent_client_protocol::schema::v1::SessionModeState>,
    ) -> Self {
        let values = configuration
            .iter()
            .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
            .collect();
        let mut surface = crate::hel_acp::surface::AcpSessionSurface::from_configuration(&values);
        surface.set_harness_kind(harness_kind);
        surface.set_config_options(config_options);
        surface.set_session_modes(modes.cloned());
        Self(surface)
    }

    pub fn supports_plan_mode(&self) -> bool {
        self.0.supports_plan_mode()
    }

    pub fn plan_mode_active(&self) -> bool {
        self.0.plan_mode_active()
    }

    pub fn supports_fast_mode(&self) -> bool {
        self.0.supports_fast_mode()
    }

    pub fn fast_mode_active(&self) -> bool {
        self.0.fast_mode_active()
    }

    pub fn current_model(&self) -> Option<&str> {
        self.0.current_model()
    }

    pub fn current_effort(&self) -> Option<&str> {
        self.0.current_effort()
    }

    /// The ACP call that turns plan mode on or off, or a sentence saying why
    /// this harness cannot.
    pub fn plan_control(&self, active: bool) -> Result<PlanControl, &'static str> {
        self.0.plan_control(active).map_err(|error| match error {
            crate::hel_acp::surface::PlanControlError::DeepseekUnsupported => {
                "Plan mode is unsupported in DSH."
            }
            crate::hel_acp::surface::PlanControlError::CodexIncompatible => {
                "This Codex ACP version does not expose collaboration_mode with plan/default values."
            }
            crate::hel_acp::surface::PlanControlError::GrokIncompatible => {
                "This Grok Build version does not expose compatible plan/default modes."
            }
            crate::hel_acp::surface::PlanControlError::Incompatible => {
                "This ACP harness does not expose compatible plan/default modes."
            }
        })
    }
}

pub fn session_config_choices(
    options: &[SessionConfigOption],
    key: &str,
) -> Vec<SessionConfigChoice> {
    let Some(option) = find_session_config_option(options, key) else {
        return Vec::new();
    };
    let SessionConfigKind::Select(select) = &option.kind else {
        return Vec::new();
    };
    let choices = match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect::<Vec<_>>(),
        SessionConfigSelectOptions::Grouped(groups) => {
            groups.iter().flat_map(|group| &group.options).collect()
        }
        _ => Vec::new(),
    };
    choices
        .into_iter()
        .map(|choice| SessionConfigChoice {
            value: choice.value.to_string(),
            name: choice.name.clone(),
            description: choice.description.clone(),
        })
        .collect()
}

pub(crate) fn find_session_config_option<'a>(
    options: &'a [SessionConfigOption],
    key: &str,
) -> Option<&'a SessionConfigOption> {
    if let Some(option) = options.iter().find(|option| option.id.to_string() == key) {
        return Some(option);
    }
    match key {
        "model" => options.iter().find(|option| {
            option.category == Some(SessionConfigOptionCategory::Model)
                && !matches!(
                    option.id.to_string().as_str(),
                    "effort" | "reasoning_effort"
                )
        }),
        "effort" => options
            .iter()
            .find(|option| option.category == Some(SessionConfigOptionCategory::ThoughtLevel))
            .or_else(|| {
                options.iter().find(|option| {
                    matches!(
                        option.id.to_string().as_str(),
                        "effort" | "reasoning_effort"
                    )
                })
            }),
        "mode" => options
            .iter()
            .find(|option| option.category == Some(SessionConfigOptionCategory::Mode)),
        _ => None,
    }
}

async fn enforce_execution_mode(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    desired: &str,
    config_options: &mut Vec<SessionConfigOption>,
    legacy_modes: &mut Option<agent_client_protocol::schema::v1::SessionModeState>,
) -> Result<()> {
    if let Some(option) = config_options.iter().find(|option| {
        option.category == Some(SessionConfigOptionCategory::Mode)
            && select_contains(&option.kind, desired)
    }) {
        let response = connection
            .send_request(SetSessionConfigOptionRequest::new(
                session_id.clone(),
                option.id.clone(),
                SessionConfigValueId::new(desired.to_string()),
            ))
            .block_task()
            .await
            .with_context(|| format!("select required ACP execution mode {desired}"))?;
        *config_options = response.config_options;
        if let Some(modes) = legacy_modes.as_mut() {
            modes.current_mode_id = desired.to_owned().into();
        }
        return Ok(());
    }
    if legacy_modes.as_ref().is_some_and(|modes| {
        modes
            .available_modes
            .iter()
            .any(|mode| mode.id.to_string() == desired)
    }) {
        connection
            .send_request(SetSessionModeRequest::new(
                session_id.clone(),
                desired.to_string(),
            ))
            .block_task()
            .await
            .with_context(|| format!("select required ACP execution mode {desired}"))?;
        if let Some(modes) = legacy_modes.as_mut() {
            modes.current_mode_id = desired.to_owned().into();
        }
        return Ok(());
    }
    bail!("ACP bridge does not expose required execution mode {desired}")
}

pub(crate) fn select_contains(kind: &SessionConfigKind, desired: &str) -> bool {
    let SessionConfigKind::Select(select) = kind else {
        return false;
    };
    match &select.options {
        agent_client_protocol::schema::v1::SessionConfigSelectOptions::Ungrouped(options) => {
            options
                .iter()
                .any(|option| option.value.to_string() == desired)
        }
        agent_client_protocol::schema::v1::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| &group.options)
            .any(|option| option.value.to_string() == desired),
        _ => false,
    }
}

#[cfg(test)]
mod tests;

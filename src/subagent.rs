//! Background subagent orchestration exposed to the primary agent as MCP.
//!
//! The primary agent spawns subagents with `create_subagent`, which returns
//! immediately. Each subagent runs to completion on its own task; when it
//! finishes, its report is pushed onto a channel the orchestrator drains and
//! injects back into the primary session as a user message.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v1::{
    HttpHeader, McpServer, McpServerHttp, SessionUpdate, StopReason, ToolCallContent,
    ToolCallStatus, UsageUpdate,
};
use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header::AUTHORIZATION};
use axum::middleware::Next;
use axum::response::Response;
use base64::Engine;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    tool, tool_router,
    transport::{
        StreamableHttpServerConfig, StreamableHttpService,
        streamable_http_server::session::local::LocalSessionManager,
    },
};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::acp::{self, AcpRuntimeConfig, RuntimeAccessMode};
use crate::agent_usage::{Record, Seat};
use crate::event::{
    InternalMessage, InternalMessageKind, PromptImage, SubagentEvent, SubagentOutcome,
    SubagentStatusKind, UiCommand, UiEvent, content_block_text,
};
use crate::ragnarok::PromptToolLifecycle;
use crate::roster::{ResolvedAgent, Roster};
use crate::trajectory::{BoundaryTracker, Checkpoint};
use crate::workspace_snapshot::{WorkspaceDelta, WorkspaceSnapshot};

pub const LABEL: &str = "subagent";
pub const MCP_SERVER_NAME: &str = "mj-subagents";

pub const DEFAULT_MAX_PARALLEL: usize = 6;
pub const MAX_PARALLEL_CAP: usize = 16;

const SERVER_DELEGATION_GUIDANCE: &str = "SUBAGENT POLICY: create_subagent launches one background subagent on a fresh ACP session with no memory of this conversation, and returns immediately. Its report arrives on its own as a user message when it finishes — never poll and never idle waiting for it. Several subagents run concurrently and all of them can write to the workspace, so give each one non-overlapping work. subagent_cancel stops one in flight or releases a finished session. You keep planning, coordination, review, verification, and the final answer.";

pub const PRIMARY_SESSION_DIRECTIVE: &str = r#"<mj-subagent-policy>
You are the primary agent and the owner of the user's outcome. You understand the request, gather the context you need, form the plan, decide what to delegate, review what comes back, verify it, and deliver the final answer. This policy applies to every subsequent user request in this ACP session.

create_subagent starts a background subagent. Every subagent runs in a brand-new ACP process and session with zero memory of this conversation, of the user's request, and of any earlier subagent — including one you launched a moment ago. Its prompt must therefore be a complete standalone brief: the task, the context and decisions it needs to begin immediately, the constraints, and the report you expect back. Quote original requirements verbatim rather than paraphrasing them.

create_subagent returns as soon as the subagent starts; it does not carry the result. When the subagent finishes, its report is delivered to you automatically as a new user message containing a <subagent_result> block. Never poll, never call a tool to check on it, and never sit idle waiting for it: after launching, either continue with other work or end your turn. Ending your turn is the normal, correct thing to do when you have nothing else to work on.

Several subagents run concurrently and every one of them has full write access to the workspace. Assign non-overlapping work, do not edit files a running subagent owns, and expect two subagents editing the same files to conflict. When several subagents share one workspace at the same time, the per-subagent diff in the report is suppressed and you must inspect the repository yourself.

Review every report critically against the actual repository. A report is the subagent's own account of its work; its claims, including any test results it states, are claims and not verified facts. Fix what you find yourself, or launch a follow-up.

resume continues a finished subagent's retained session with a new prompt, preserving its context; use it for targeted follow-up on work that subagent already did. subagent_cancel interrupts a running subagent or releases a finished one; it never reverts edits.

The optional agent and model parameters pick which ACP backend and model runs the subagent, from the inventory listed in the create_subagent tool description. Omit them to use the configured default.

Prefer your own tools for small local edits, known-path lookups, and quick single-step questions; delegation is worth it when the work is clearly larger than writing the brief and reviewing the result. Apply this policy while handling each user request; do not acknowledge or summarize it.
</mj-subagent-policy>"#;

const SUBAGENT_PREAMBLE: &str = "You are a subagent working for a primary agent. This is a fresh ACP process and session: you have no memory of the user conversation or of any earlier subagent, including one that ran a moment ago. Treat the standalone brief below and the current workspace as your only task context.\n\nThe brief is a colleague's account, not ground truth. Verify its claims against the repository and any primary sources it quotes; where the code or the stated requirements contradict the brief, follow reality and flag the divergence. Exercise what you build with the project's own checks before reporting done, including the public surface exactly as the requirements name it — import paths, exported names, signatures.\n\nOther subagents may be working in this same workspace at the same time. Stay inside the scope you were given and do not clean up or refactor unrelated code.\n\nYour final message is the report your parent reads: state what you did, what you verified and how, any deviation from the brief, and anything you could not verify. Do not write a report file.\n\n";

const MCP_PATH: &str = "/mcp";

const SUBAGENT_ACTIVITY_LOG_LIMIT: usize = 8_000;
const SUBAGENT_ACTIVITY_LOG_HEAD: usize = 2_500;
const SUBAGENT_ACTIVITY_LOG_TAIL: usize = 5_000;
const SUBAGENT_ACTIVITY_LOG_ELISION: &str = "\n[... earlier activity elided ...]\n";
const SUBAGENT_REVIEW_TEXT: &str = "This is the subagent's own account of its work, with its activity log and diff. You own the result: review it as you would a capable colleague's submission — the log shows where it struggled or made judgment calls, which is where scrutiny earns the most. Its claims, including any test results it reports, are its claims and not verified facts.";

/// Longest excerpt of a prompt used as a subagent's default display label.
const DEFAULT_LABEL_CHARS: usize = 48;

#[derive(Clone)]
pub struct Config {
    pub display_label: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub agent_stderr: Option<PathBuf>,
    pub role_config: Option<acp::RuntimeRoleConfig>,
    pub subagent_handoff_counter: Option<Arc<AtomicUsize>>,
    pub active_implementation_workers: ActiveSubagentWorkers,
    pub max_parallel: usize,
    pub snapshot_exclusions: Vec<PathBuf>,
    /// Id source installed on the controller when the MCP server starts, so
    /// discrete-review lanes can draw from the same sequence.
    pub id_allocator: SubagentIdAllocator,
    headless_permission_mode: Option<crate::config::PermissionPreset>,
    role_pool: Option<crate::quota::RolePool>,
    quota_gate: Option<crate::quota::Gate>,
    inventory: Arc<RwLock<SubagentInventory>>,
    reports: Option<SubagentReportBus>,
    preamble: String,
    mcp_servers: Vec<McpServer>,
    usage_seat: Seat,
    retain_after_completion: bool,
    warm: Arc<WarmPool>,
}

#[derive(Default)]
struct WarmPool {
    slot: StdMutex<Option<WarmRuntime>>,
}

struct WarmRuntime {
    context: RunContext,
    role_key: String,
    events: mpsc::UnboundedReceiver<UiEvent>,
    commands: mpsc::UnboundedSender<UiCommand>,
    task: JoinHandle<Result<()>>,
    cancel: CancellationToken,
}

impl Drop for WarmPool {
    fn drop(&mut self) {
        let slot = self.slot.get_mut().expect("subagent warm pool poisoned");
        if let Some(runtime) = slot.as_ref() {
            runtime.cancel.cancel();
            let _ = runtime.commands.send(UiCommand::Shutdown);
        }
    }
}

impl Config {
    pub fn new(role_pool: crate::quota::RolePool, agent_stderr: Option<PathBuf>) -> Self {
        let role = role_pool.current();
        Self::from_role(role, agent_stderr, Some(role_pool))
    }

    /// Build a pool pinned to one exact resolved role.
    ///
    /// Review supervision uses this to stay on the primary model instead of
    /// entering the worker pool's failover ladder.
    pub(crate) fn for_resolved_agent(role: ResolvedAgent, agent_stderr: Option<PathBuf>) -> Self {
        Self::from_role(role, agent_stderr, None)
    }

    fn from_role(
        role: ResolvedAgent,
        agent_stderr: Option<PathBuf>,
        role_pool: Option<crate::quota::RolePool>,
    ) -> Self {
        let reasoning_effort = role.reasoning_effort.clone();
        Self {
            display_label: format!("subagent · {}", role.model.model),
            command: role.launch.command,
            args: role.launch.args,
            env: role.launch.env,
            agent_stderr,
            role_config: Some(acp::RuntimeRoleConfig {
                label: LABEL.to_string(),
                model_id: role.model.model,
                model_value: role.model_value,
                adapter_source_id: role.launch.source_id,
                permission: None,
                session_tag: None,
                reasoning_effort,
            }),
            subagent_handoff_counter: None,
            active_implementation_workers: ActiveSubagentWorkers::default(),
            max_parallel: DEFAULT_MAX_PARALLEL,
            snapshot_exclusions: Vec::new(),
            id_allocator: SubagentIdAllocator::default(),
            headless_permission_mode: None,
            role_pool,
            quota_gate: None,
            inventory: Arc::default(),
            reports: None,
            preamble: SUBAGENT_PREAMBLE.to_string(),
            mcp_servers: Vec::new(),
            usage_seat: Seat::Subagent,
            retain_after_completion: true,
            warm: Arc::default(),
        }
    }

    pub fn with_subagent_handoff_counter(mut self, counter: Arc<AtomicUsize>) -> Self {
        self.subagent_handoff_counter = Some(counter);
        self
    }

    /// Share one id sequence with the discrete-review fan-out so pool subagents
    /// and review lanes never render under the same status-row id.
    pub fn with_id_allocator(mut self, allocator: SubagentIdAllocator) -> Self {
        self.id_allocator = allocator;
        self
    }

    pub fn with_active_implementation_workers(mut self, workers: ActiveSubagentWorkers) -> Self {
        self.active_implementation_workers = workers;
        self
    }

    pub fn with_max_parallel(mut self, max: usize) -> Self {
        self.max_parallel = max.clamp(1, MAX_PARALLEL_CAP);
        self
    }

    pub fn with_headless_permission_mode(mut self, mode: crate::config::PermissionPreset) -> Self {
        self.headless_permission_mode = Some(mode);
        self
    }

    pub fn with_quota_gate(mut self, gate: crate::quota::Gate) -> Self {
        self.quota_gate = Some(gate);
        self
    }

    pub fn with_reports(mut self, reports: SubagentReportBus) -> Self {
        self.reports = Some(reports);
        self
    }

    /// Customize the standalone instructions prepended to every fresh run.
    pub(crate) fn with_preamble(mut self, preamble: impl Into<String>) -> Self {
        self.preamble = preamble.into();
        self
    }

    /// Attach fixed MCP servers to runs launched from this configuration.
    ///
    /// Nested runs never receive Mjolnir's generic subagent server; only these
    /// explicitly supplied servers are advertised.
    pub(crate) fn with_mcp_servers(mut self, servers: Vec<McpServer>) -> Self {
        self.mcp_servers = servers;
        self
    }

    pub(crate) fn with_usage_seat(mut self, seat: Seat) -> Self {
        self.usage_seat = seat;
        self
    }

    pub(crate) fn with_retain_after_completion(mut self, retain: bool) -> Self {
        self.retain_after_completion = retain;
        self
    }

    /// Shared inventory handle so the caller can keep refreshing the advertised
    /// agents and models as background adapter probes land.
    pub fn with_inventory(mut self, inventory: Arc<RwLock<SubagentInventory>>) -> Self {
        self.inventory = inventory;
        self
    }

    pub fn with_prewarm(mut self, context: RunContext) -> Self {
        self.snapshot_exclusions = context.snapshot_exclusions.clone();
        self.ensure_warm(context);
        self
    }

    fn ensure_warm(&self, context: RunContext) {
        let mut slot = self.warm.slot.lock().expect("subagent warm pool poisoned");
        let role_key = self.role_key();
        if slot
            .as_ref()
            .is_some_and(|runtime| runtime.context != context || runtime.role_key != role_key)
        {
            let stale = slot.take().expect("checked warm slot disappeared");
            stale.cancel.cancel();
            let _ = stale.commands.send(UiCommand::Shutdown);
        }
        if slot.is_none() {
            *slot = Some(spawn_subagent_runtime(
                self,
                context,
                None,
                &self.mcp_servers,
            ));
        }
    }

    fn take_warm(&self, context: &RunContext) -> Option<WarmRuntime> {
        let mut slot = self.warm.slot.lock().expect("subagent warm pool poisoned");
        if slot
            .as_ref()
            .is_some_and(|runtime| runtime.task.is_finished())
        {
            let failed = slot.take().expect("finished warm slot disappeared");
            failed.cancel.cancel();
            let _ = failed.commands.send(UiCommand::Shutdown);
        }
        let role_key = self.role_key();
        if slot
            .as_ref()
            .is_some_and(|runtime| runtime.context == *context && runtime.role_key == role_key)
        {
            slot.take()
        } else {
            None
        }
    }

    fn role_key(&self) -> String {
        self.role_config
            .as_ref()
            .map(|role| {
                format!(
                    "{}\0{}\0{:?}",
                    role.adapter_source_id, role.model_id, self.headless_permission_mode
                )
            })
            .unwrap_or_else(|| self.display_label.clone())
    }

    fn apply_role(&mut self, role: ResolvedAgent) {
        self.display_label = format!("subagent · {}", role.model.model);
        self.command = role.launch.command;
        self.args = role.launch.args;
        self.env = role.launch.env;
        let session_tag = self
            .role_config
            .as_ref()
            .and_then(|config| config.session_tag.clone());
        let reasoning_effort = role.reasoning_effort.clone();
        self.role_config = Some(acp::RuntimeRoleConfig {
            label: LABEL.to_string(),
            model_id: role.model.model,
            model_value: role.model_value,
            adapter_source_id: role.launch.source_id,
            permission: None,
            session_tag,
            reasoning_effort,
        });
    }

    fn current_agent(&self) -> String {
        self.role_config
            .as_ref()
            .map(|role| role.adapter_source_id.clone())
            .unwrap_or_default()
    }

    fn current_model(&self) -> String {
        self.role_config
            .as_ref()
            .map(|role| role.model_id.clone())
            .unwrap_or_default()
    }

    fn rendered_inventory(&self) -> String {
        self.inventory
            .read()
            .expect("subagent inventory poisoned")
            .render(&self.current_agent(), &self.current_model())
    }

    /// Resolves the caller's optional `agent`/`model` pair to the ACP session a
    /// run should use. Both omitted keeps today's behavior: the configured
    /// `RolePool` picks (and can fail over) at worker start. An explicit pick
    /// bypasses the pool but still consults the quota gate once, so a blocked
    /// provider fails fast instead of stalling inside the adapter.
    fn resolve_session(
        &self,
        agent: Option<&str>,
        model: Option<&str>,
    ) -> std::result::Result<SessionSpec, McpError> {
        if agent.is_none() && model.is_none() {
            return Ok(SessionSpec {
                agent: self.current_agent(),
                model: self.current_model(),
                role: None,
            });
        }
        let inventory = self
            .inventory
            .read()
            .expect("subagent inventory poisoned")
            .clone();
        let role = inventory.resolve(agent, model)?;
        Ok(SessionSpec {
            agent: role.launch.source_id.clone(),
            model: role.model.model.clone(),
            role: Some(Box::new(role)),
        })
    }

    async fn check_explicit_quota(
        &self,
        role: &ResolvedAgent,
    ) -> std::result::Result<(), McpError> {
        let Some(gate) = self.quota_gate.as_ref() else {
            return Ok(());
        };
        if let crate::quota::Check::NearLimit { .. } = gate.check(role).await {
            return Err(McpError::invalid_params(
                format!(
                    "{} quota has 5% or less remaining, so {} was not started; pick another agent or omit agent and model to use the configured default",
                    role.launch.source_id, role.model.model
                ),
                None,
            ));
        }
        Ok(())
    }
}

/// Which ACP session a run should use. `role: None` means "let the configured
/// `RolePool` choose at worker start", preserving quota failover for the
/// default path.
#[derive(Debug, Clone)]
struct SessionSpec {
    agent: String,
    model: String,
    role: Option<Box<ResolvedAgent>>,
}

/// Observable lifetime of subagent workers. The count reaches zero only after
/// every supervisor has reaped its ACP process tree and released its
/// controller lease. Retained (finished, idle) sessions do not count.
#[derive(Clone, Debug)]
pub struct ActiveSubagentWorkers {
    updates: watch::Sender<usize>,
}

impl Default for ActiveSubagentWorkers {
    fn default() -> Self {
        let (updates, _) = watch::channel(0);
        Self { updates }
    }
}

impl ActiveSubagentWorkers {
    pub fn subscribe(&self) -> watch::Receiver<usize> {
        self.updates.subscribe()
    }

    pub(crate) fn set(&self, count: usize) {
        self.updates.send_replace(count);
    }
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

/// One finished subagent turn, pushed to the orchestrator for injection into
/// the primary session.
#[derive(Debug, Clone)]
pub struct SubagentReport {
    pub subagent_id: u64,
    pub label: String,
    pub agent: String,
    pub model: String,
    pub outcome: SubagentOutcome,
    pub final_message: String,
    pub slim_activity: String,
    /// `None` when no snapshot was available; `Some` carries either the diff or
    /// the note explaining why it was omitted.
    pub workspace_diff: Option<String>,
    pub elapsed: Duration,
}

/// The channel finished subagent reports travel on, plus the outstanding-report
/// counter headless uses to decide when it is safe to exit. `open` happens at
/// admission -- synchronously inside `create_subagent`, so it is always visible
/// before the primary's turn can complete -- and `close` once the orchestrator
/// has handled the matching report.
#[derive(Clone, Debug)]
pub struct SubagentReportBus {
    tx: mpsc::UnboundedSender<SubagentReport>,
    pending: Arc<AtomicUsize>,
}

impl SubagentReportBus {
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<SubagentReport>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                pending: Arc::new(AtomicUsize::new(0)),
            },
            rx,
        )
    }

    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    pub(crate) fn open(&self) {
        self.pending.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn deliver(&self, report: SubagentReport) {
        if self.tx.send(report).is_err() {
            self.close();
        }
    }

    /// Called by the orchestrator once a report has been handled (injected or
    /// deliberately dropped).
    pub fn close(&self) {
        let _ = self
            .pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_sub(1))
            });
    }
}

/// Format a batch of completed runs for injection into their coordinator's
/// next turn.
pub(crate) fn format_report_injection(
    reports: &[SubagentReport],
    trailing_instruction: &str,
) -> String {
    let mut out = String::new();
    for report in reports {
        let diff = report
            .workspace_diff
            .as_deref()
            .unwrap_or("[workspace snapshot unavailable for this subagent]");
        out.push_str(&format!(
            "<subagent_result id=\"{id}\" label=\"{label}\" agent=\"{agent}\" model=\"{model}\" outcome=\"{outcome}\" elapsed=\"{elapsed}\">\n<report>\n{report_text}\n</report>\n<activity_summary>\n{activity}\n</activity_summary>\n<workspace_diff>\n{diff}\n</workspace_diff>\n</subagent_result>\n\n",
            id = report.subagent_id,
            label = escape_report_attribute(&report.label),
            agent = escape_report_attribute(&report.agent),
            model = escape_report_attribute(&report.model),
            outcome = report.outcome.label(),
            elapsed = format_report_elapsed(report.elapsed),
            report_text = report.final_message.trim(),
            activity = report.slim_activity.trim(),
        ));
    }
    out.push_str(trailing_instruction);
    out
}

fn escape_report_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace(['\n', '\r'], " ")
}

fn format_report_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

// ---------------------------------------------------------------------------
// Inventory
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SubagentInventory {
    pub servers: Vec<SubagentServer>,
    pub default_label: String,
    roles: Vec<ResolvedAgent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentServer {
    pub id: String,
    pub label: String,
    pub models: Vec<SubagentModel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentModel {
    pub id: String,
    pub value: String,
    pub ranked: bool,
    pub default: bool,
}

impl SubagentInventory {
    /// Groups every launchable role by the ACP server that advertises it. The
    /// `(role, launch.source_id)` pairing already exists on `ResolvedAgent`;
    /// this only reshapes it for per-call selection and tool-description
    /// rendering.
    pub fn from_roster(roster: &Roster) -> Self {
        let default = roster.subagent_default.as_ref();
        let mut servers: Vec<SubagentServer> = Vec::new();
        for role in &roster.available {
            let is_default = default.is_some_and(|other| {
                other.model.model == role.model.model
                    && other.launch.source_id == role.launch.source_id
            });
            let model = SubagentModel {
                id: role.model.model.clone(),
                value: role.model_value.clone(),
                ranked: role.ranked,
                default: is_default,
            };
            match servers
                .iter_mut()
                .find(|server| server.id == role.launch.source_id)
            {
                Some(server) => {
                    if !server.models.iter().any(|other| other.id == model.id) {
                        server.models.push(model);
                    }
                }
                None => servers.push(SubagentServer {
                    id: role.launch.source_id.clone(),
                    label: crate::roster::AdapterKind::from_source_id(&role.launch.source_id)
                        .map(|kind| kind.display_name().to_string())
                        .unwrap_or_else(|| role.launch.source_id.clone()),
                    models: vec![model],
                }),
            }
        }
        Self {
            servers,
            default_label: default
                .map(|role| role.model.model.clone())
                .unwrap_or_default(),
            roles: roster.available.clone(),
        }
    }

    fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// The block appended to `create_subagent`'s description and to the MCP
    /// server instructions.
    pub fn render(&self, default_agent: &str, default_model: &str) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut out = String::from("Available agents and models:");
        for server in &self.servers {
            let models = server
                .models
                .iter()
                .map(|model| {
                    let is_default = model.default
                        || (server.id == default_agent && model.id == default_model)
                        || (!self.default_label.is_empty() && model.id == self.default_label);
                    if is_default {
                        format!("{}*", model.id)
                    } else {
                        model.id.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!("\n- {} ({}): {}", server.id, server.label, models));
        }
        out.push_str("\n(* = default when agent and model are omitted)");
        out
    }

    fn valid_options(&self) -> String {
        let agents = self
            .servers
            .iter()
            .map(|server| server.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let models = self
            .servers
            .iter()
            .flat_map(|server| server.models.iter().map(|model| model.id.as_str()))
            .collect::<Vec<_>>()
            .join(", ");
        format!("valid agents: [{agents}]; valid models: [{models}]")
    }

    fn server_for(&self, agent: &str) -> Option<&SubagentServer> {
        self.servers
            .iter()
            .find(|server| server.id.eq_ignore_ascii_case(agent))
    }

    fn role(&self, agent: &str, model_id: &str) -> Option<ResolvedAgent> {
        self.roles
            .iter()
            .find(|role| {
                role.launch.source_id.eq_ignore_ascii_case(agent) && role.model.model == model_id
            })
            .cloned()
    }

    /// `model` matches on the model id first, then on the raw advertised value,
    /// both case-insensitively.
    fn find_model<'a>(server: &'a SubagentServer, model: &str) -> Option<&'a SubagentModel> {
        server
            .models
            .iter()
            .find(|candidate| candidate.id.eq_ignore_ascii_case(model))
            .or_else(|| {
                server
                    .models
                    .iter()
                    .find(|candidate| candidate.value.eq_ignore_ascii_case(model))
            })
    }

    fn best_model(server: &SubagentServer) -> Option<&SubagentModel> {
        server
            .models
            .iter()
            .find(|model| model.default)
            .or_else(|| server.models.iter().find(|model| model.ranked))
            .or_else(|| server.models.first())
    }

    fn resolve(
        &self,
        agent: Option<&str>,
        model: Option<&str>,
    ) -> std::result::Result<ResolvedAgent, McpError> {
        if self.is_empty() {
            return Err(McpError::invalid_params(
                "no subagent agents or models are currently launchable; omit agent and model to use the configured default",
                None,
            ));
        }
        match (agent, model) {
            (Some(agent), Some(model)) => {
                let server = self
                    .server_for(agent)
                    .ok_or_else(|| self.unknown_agent(agent))?;
                let candidate = Self::find_model(server, model).ok_or_else(|| {
                    McpError::invalid_params(
                        format!(
                            "agent {agent} does not advertise model {model}; {}",
                            self.valid_options()
                        ),
                        None,
                    )
                })?;
                self.role(&server.id, &candidate.id)
                    .ok_or_else(|| self.unresolvable(&server.id, &candidate.id))
            }
            (Some(agent), None) => {
                let server = self
                    .server_for(agent)
                    .ok_or_else(|| self.unknown_agent(agent))?;
                let candidate = Self::best_model(server).ok_or_else(|| {
                    McpError::invalid_params(
                        format!("agent {agent} advertises no launchable model"),
                        None,
                    )
                })?;
                self.role(&server.id, &candidate.id)
                    .ok_or_else(|| self.unresolvable(&server.id, &candidate.id))
            }
            (None, Some(model)) => {
                let hit = self.servers.iter().find_map(|server| {
                    Self::find_model(server, model).map(|candidate| (server, candidate))
                });
                let Some((server, candidate)) = hit else {
                    return Err(McpError::invalid_params(
                        format!(
                            "no agent advertises model {model}; {}",
                            self.valid_options()
                        ),
                        None,
                    ));
                };
                self.role(&server.id, &candidate.id)
                    .ok_or_else(|| self.unresolvable(&server.id, &candidate.id))
            }
            (None, None) => unreachable!("the default path is resolved before reaching here"),
        }
    }

    fn unknown_agent(&self, agent: &str) -> McpError {
        McpError::invalid_params(
            format!("unknown agent {agent}; {}", self.valid_options()),
            None,
        )
    }

    fn unresolvable(&self, agent: &str, model: &str) -> McpError {
        McpError::invalid_params(
            format!("{agent}/{model} is advertised but not currently launchable"),
            None,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunContext {
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub snapshot_exclusions: Vec<PathBuf>,
    pub fs_max_text_bytes: u64,
    pub access_mode: RuntimeAccessMode,
}

/// One fixed job launched by a Mjolnir-owned coordinator.
///
/// This is deliberately role-neutral: a review supervisor and its reviewers
/// are peers at the runner layer even though their orchestration roles differ.
#[derive(Debug, Clone)]
pub(crate) struct ProgrammaticJob {
    pub prompt: String,
    pub images: Vec<PromptImage>,
    pub label: String,
    pub preamble: String,
    pub mcp_servers: Vec<McpServer>,
    pub retain_after_completion: bool,
    pub workflow: Option<crate::workflow::WorkflowActorContext>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProgrammaticStarted {
    pub subagent_id: u64,
    pub agent: String,
    pub model: String,
}

#[derive(Clone)]
struct RunPolicy {
    preamble: String,
    mcp_servers: Vec<McpServer>,
    usage_seat: Seat,
    retain_after_completion: bool,
    allow_warm_runtime: bool,
    /// Programmatic retained agents are coordinators whose identity should
    /// remain visible while they wait for another injected turn. Public MCP
    /// subagents keep their existing per-turn `Finished` behavior.
    defer_finished_while_retained: bool,
    workflow: Option<crate::workflow::WorkflowActorContext>,
}

impl RunPolicy {
    fn configured(config: &Config) -> Self {
        Self {
            preamble: config.preamble.clone(),
            mcp_servers: config.mcp_servers.clone(),
            usage_seat: config.usage_seat,
            retain_after_completion: config.retain_after_completion,
            allow_warm_runtime: true,
            defer_finished_while_retained: false,
            workflow: None,
        }
    }

    fn programmatic(config: &Config, job: &ProgrammaticJob) -> Self {
        Self {
            preamble: job.preamble.clone(),
            mcp_servers: job.mcp_servers.clone(),
            usage_seat: config.usage_seat,
            retain_after_completion: job.retain_after_completion,
            // A prewarmed process has already completed session/new with its
            // MCP list, so a job-specific list always requires a fresh runtime.
            allow_warm_runtime: false,
            defer_finished_while_retained: job.retain_after_completion,
            workflow: job.workflow.clone(),
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateSubagentArgs {
    /// Complete, standalone brief for the subagent.
    pub prompt: String,
    /// Optional ACP server id from the inventory in this tool's description.
    #[serde(default)]
    pub agent: Option<String>,
    /// Optional model id from the inventory in this tool's description.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional short display label for this subagent.
    #[serde(default)]
    pub label: Option<String>,
    /// Optional absolute working directory inside the authorized roots.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Optional finished subagent id whose retained session continues with this prompt.
    #[serde(default)]
    pub resume: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubagentCancelArgs {
    /// Subagent id returned by create_subagent.
    pub subagent_id: u64,
}

#[derive(Clone)]
struct McpHandler {
    config: Config,
    context: RunContext,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    controller: Controller,
    runs: SubagentRegistry,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl McpHandler {
    fn new(
        config: Config,
        context: RunContext,
        ui_tx: mpsc::UnboundedSender<UiEvent>,
        controller: Controller,
    ) -> Self {
        Self {
            config,
            context,
            ui_tx,
            controller,
            runs: SubagentRegistry::default(),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "create_subagent",
        description = "LAUNCH A BACKGROUND SUBAGENT. Starts one subagent on a fresh ACP process and session and RETURNS IMMEDIATELY with its subagentId; it does not carry the result. The subagent's report arrives on its own as a new user message containing a <subagent_result> block when it finishes. Never poll, never wait idle, never call another tool to check on it: after launching, do other work or end your turn. The subagent has zero memory of this conversation, so `prompt` must be a complete standalone brief: the task, the context and decisions needed to start immediately, the constraints, and the report you expect. Several subagents run concurrently and ALL of them can write to the workspace, so give each one non-overlapping work and do not edit files a running subagent owns. Optional `agent` and `model` pick the backend from the inventory below; omit them for the default. Optional `label` is a short display name. Optional `cwd` must be an absolute directory inside the authorized workspace roots. Optional `resume` continues a finished subagent's retained session with this prompt instead of starting a fresh one. Prefer your own tools for small edits and quick lookups."
    )]
    async fn create_subagent(
        &self,
        Parameters(args): Parameters<CreateSubagentArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if args.prompt.trim().is_empty() {
            return Err(McpError::invalid_params("prompt must not be empty", None));
        }
        let context = resolve_subagent_context(&self.context, args.cwd.as_deref()).await?;
        let spec = self
            .config
            .resolve_session(args.agent.as_deref(), args.model.as_deref())?;
        if let Some(role) = spec.role.as_deref() {
            self.config.check_explicit_quota(role).await?;
        }
        let label = args
            .label
            .as_deref()
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| default_label(&args.prompt));

        if let Some(subagent_id) = args.resume {
            return self
                .resume_subagent(subagent_id, args.prompt, &label, &spec)
                .await;
        }

        let subagent_id = match admit_and_launch_run(
            &self.controller,
            &self.runs,
            &self.config,
            context,
            args.prompt,
            Vec::new(),
            label.clone(),
            spec.clone(),
            RunPolicy::configured(&self.config),
            &self.ui_tx,
        )
        .await
        {
            Ok(subagent_id) => subagent_id,
            Err(full) => {
                return Ok(CallToolResult::error(vec![Content::text(full.message())]));
            }
        };
        self.note_handoff();
        Ok(started_tool_result(
            subagent_id,
            &label,
            &spec.agent,
            &spec.model,
        ))
    }

    async fn resume_subagent(
        &self,
        subagent_id: u64,
        prompt: String,
        label: &str,
        spec: &SessionSpec,
    ) -> std::result::Result<CallToolResult, McpError> {
        if let Err(failure) = resume_retained_run(
            &self.controller,
            &self.runs,
            &self.config,
            subagent_id,
            prompt,
        )
        .await
        {
            if failure == ResumeFailure::Unknown {
                return Err(McpError::invalid_params(failure.message(subagent_id), None));
            }
            return Ok(CallToolResult::error(vec![Content::text(
                failure.message(subagent_id),
            )]));
        }
        self.note_handoff();
        Ok(started_tool_result(
            subagent_id,
            label,
            &spec.agent,
            &spec.model,
        ))
    }

    /// Counts one delegation for the turn. Every admitted spawn counts,
    /// including a `resume` that re-admits a retained session, because the
    /// discrete-review gate asks "did this turn delegate at all".
    fn note_handoff(&self) {
        if let Some(counter) = self.config.subagent_handoff_counter.as_ref() {
            counter.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[tool(
        name = "subagent_cancel",
        description = "STOP OR RELEASE A SUBAGENT (subagent_id from create_subagent). On a running subagent this interrupts its in-flight turn and returns what it did, plus the workspace diff as it left it. On a finished, retained subagent it releases the idle session. Either way it does NOT revert changes the subagent already made: its edits remain in the workspace exactly as it left them, so you can review or finish the work yourself. No report is injected for a cancelled subagent; this tool result is the whole story. Calling this with an unknown or already-released subagent_id fails."
    )]
    async fn subagent_cancel(
        &self,
        Parameters(args): Parameters<SubagentCancelArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let Some(run) = self.runs.take(args.subagent_id) else {
            return Err(McpError::invalid_params(
                unresolved_subagent_message(args.subagent_id),
                None,
            ));
        };
        let (respond, respond_rx) = oneshot::channel();
        if run.control.send(WorkerRequest::Cancel { respond }).is_err() {
            return Ok(CallToolResult::error(vec![Content::text(
                worker_unavailable_message(args.subagent_id),
            )]));
        }
        Ok(match respond_rx.await {
            Ok(result) => cancelled_tool_result(&result),
            Err(_) => CallToolResult::error(vec![Content::text(format!(
                "subagent #{} was cancelled, but its worker ended before confirming teardown. Any partial edits remain in the workspace exactly as it left them.",
                args.subagent_id
            ))]),
        })
    }
}

fn default_label(prompt: &str) -> String {
    let first = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("subagent");
    let mut label: String = first.chars().take(DEFAULT_LABEL_CHARS).collect();
    if first.chars().count() > DEFAULT_LABEL_CHARS {
        label.push('…');
    }
    label
}

fn started_tool_result(subagent_id: u64, label: &str, agent: &str, model: &str) -> CallToolResult {
    let text = format!(
        "subagent #{subagent_id} ({label}) started on {agent}/{model}. It is running in the background and this call carries no result. Its report arrives on its own as a user message containing a <subagent_result id=\"{subagent_id}\"> block when it finishes. Do not poll and do not wait idle for it: continue with other work or end your turn. subagent_cancel with subagent_id {subagent_id} stops it."
    );
    let mut result = CallToolResult::success(vec![Content::text(text)]);
    result.structured_content = Some(serde_json::json!({
        "subagentId": subagent_id,
        "status": "started",
        "agent": agent,
        "model": model,
        "label": label,
    }));
    result
}

/// Narrows an explicit subagent launch to its requested worktree. The outer
/// runtime has already authorized `cwd` and `additional_directories`; a
/// subagent cannot use those roots to reach an arbitrary sibling.
async fn resolve_subagent_context(
    outer: &RunContext,
    delegated_cwd: Option<&Path>,
) -> std::result::Result<RunContext, McpError> {
    let Some(delegated_cwd) = delegated_cwd else {
        return Ok(outer.clone());
    };
    if !delegated_cwd.is_absolute() {
        return Err(McpError::invalid_params(
            "cwd must be an absolute path",
            None,
        ));
    }
    let delegated_cwd = tokio::fs::canonicalize(delegated_cwd)
        .await
        .map_err(|error| {
            McpError::invalid_params(
                format!("cwd must be an existing, accessible directory: {error}"),
                None,
            )
        })?;
    if !tokio::fs::metadata(&delegated_cwd)
        .await
        .map_err(|error| {
            McpError::invalid_params(
                format!("cwd must be an existing, accessible directory: {error}"),
                None,
            )
        })?
        .is_dir()
    {
        return Err(McpError::invalid_params(
            "cwd must be an existing directory",
            None,
        ));
    }

    let mut authorized_roots = Vec::with_capacity(1 + outer.additional_directories.len());
    authorized_roots.push(outer.cwd.clone());
    authorized_roots.extend(outer.additional_directories.iter().cloned());
    let mut contains_delegated_cwd = false;
    for root in authorized_roots {
        let root = tokio::fs::canonicalize(&root).await.map_err(|error| {
            McpError::invalid_params(
                format!("configured workspace root is inaccessible: {error}"),
                None,
            )
        })?;
        if delegated_cwd.starts_with(root) {
            contains_delegated_cwd = true;
            break;
        }
    }
    if !contains_delegated_cwd {
        return Err(McpError::invalid_params(
            format!(
                "cwd {} is outside the authorized workspace roots; create_subagent may only launch within the current workspace root or configured additional workspace roots. Configure the target as an additional workspace root first",
                delegated_cwd.display()
            ),
            None,
        ));
    }

    Ok(RunContext {
        cwd: delegated_cwd,
        additional_directories: Vec::new(),
        snapshot_exclusions: outer.snapshot_exclusions.clone(),
        fs_max_text_bytes: outer.fs_max_text_bytes,
        access_mode: outer.access_mode,
    })
}

/// Returns the Git roots whose changes belong to one subagent run. An explicit
/// `cwd` has already been narrowed by `resolve_subagent_context`, so this
/// deliberately cannot reach outer siblings.
fn subagent_workspace_roots(context: &RunContext) -> Vec<PathBuf> {
    let mut roots = Vec::with_capacity(1 + context.additional_directories.len());
    roots.push(context.cwd.clone());
    roots.extend(context.additional_directories.iter().cloned());
    roots
}

async fn capture_workspace_snapshot(context: &RunContext) -> WorkspaceSnapshot {
    WorkspaceSnapshot::capture_excluding(
        &subagent_workspace_roots(context),
        &context.snapshot_exclusions,
    )
    .await
}

async fn canonical_root(cwd: &Path) -> PathBuf {
    tokio::fs::canonicalize(cwd)
        .await
        .unwrap_or_else(|_| cwd.to_path_buf())
}

fn spawn_subagent_runtime(
    config: &Config,
    context: RunContext,
    termination: Option<CancellationToken>,
    mcp_servers: &[McpServer],
) -> WarmRuntime {
    let (event_tx, events) = mpsc::unbounded_channel();
    let (commands, command_rx) = mpsc::unbounded_channel();
    let cancel = termination.unwrap_or_default();
    let mut env = config.env.clone();
    let mut role_config = config.role_config.clone();
    if let Some(mode) = config.headless_permission_mode
        && let Some(role) = role_config.as_mut()
        && let Some(kind) = crate::roster::AdapterKind::from_source_id(&role.adapter_source_id)
    {
        role.permission = crate::roster::configure_permissions(kind, mode, &mut env);
    }
    let runtime_config = AcpRuntimeConfig {
        command: config.command.clone(),
        args: config.args.clone(),
        cwd: context.cwd.clone(),
        additional_directories: context.additional_directories.clone(),
        mcp_servers: mcp_servers.to_vec(),
        resume_session: None,
        session_restore_mode: crate::acp::SessionRestoreMode::Continue,
        env,
        agent_stderr: config.agent_stderr.clone(),
        fs_max_text_bytes: context.fs_max_text_bytes,
        access_mode: context.access_mode,
        agent_source_id: None,
        config_path: None,
        saved_session_config: HashMap::new(),
        role_config,
        subagents: None,
        side_prompt_policy: false,
        termination: Some(cancel.clone()),
    };
    let task = tokio::spawn(acp::run(runtime_config, event_tx, command_rx));
    WarmRuntime {
        context,
        role_key: config.role_key(),
        events,
        commands,
        task,
        cancel,
    }
}

impl McpHandler {
    fn server_info(&self) -> ServerInfo {
        let inventory = self.config.rendered_inventory();
        let instructions = if inventory.is_empty() {
            format!("{SERVER_DELEGATION_GUIDANCE}\n\n{PRIMARY_SESSION_DIRECTIVE}")
        } else {
            format!("{SERVER_DELEGATION_GUIDANCE}\n\n{inventory}\n\n{PRIMARY_SESSION_DIRECTIVE}")
        };
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                MCP_SERVER_NAME,
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(instructions)
    }

    /// `list_tools` is hand-implemented so `create_subagent`'s description can
    /// carry the live agent/model inventory. Adapters that snapshot the tool
    /// list at session start see the startup inventory only.
    fn described_tools(&self) -> Vec<Tool> {
        let inventory = self.config.rendered_inventory();
        self.tool_router
            .list_all()
            .into_iter()
            .map(|mut tool| {
                if tool.name == "create_subagent" && !inventory.is_empty() {
                    let base = tool.description.as_deref().unwrap_or_default().to_string();
                    tool.description = Some(format!("{base}\n\n{inventory}").into());
                }
                tool
            })
            .collect()
    }
}

impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerInfo {
        self.server_info()
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = std::result::Result<ListToolsResult, McpError>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(self.described_tools())))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = std::result::Result<CallToolResult, McpError>> + Send + '_ {
        self.tool_router
            .call(ToolCallContext::new(self, request, context))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.described_tools()
            .into_iter()
            .find(|tool| tool.name == name)
    }
}

/// In-process, loopback-only MCP endpoint advertised to the primary ACP agent.
/// Dropping it cancels the listener and every open MCP session.
pub struct HttpServer {
    advertised: McpServer,
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

impl HttpServer {
    pub async fn start(
        config: Config,
        context: RunContext,
        ui_tx: mpsc::UnboundedSender<UiEvent>,
        controller: Controller,
    ) -> Result<Self> {
        controller
            .configure(
                config.max_parallel,
                config.active_implementation_workers.clone(),
                config.id_allocator.clone(),
            )
            .await;
        let mut token_bytes = [0_u8; 32];
        getrandom::fill(&mut token_bytes)
            .map_err(|error| anyhow!("generate subagent MCP bearer token: {error}"))?;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token_bytes);
        let authorization = format!("Bearer {token}");

        let handler = McpHandler::new(config, context, ui_tx, controller);
        let cancellation = CancellationToken::new();
        let mut server_config = StreamableHttpServerConfig::default();
        server_config.cancellation_token = cancellation.clone();
        // rmcp's LocalSessionManager evicts idle sessions after
        // SessionConfig::keep_alive (default 300s). A retained subagent session
        // idle past that would 404 every later resume or cancel call. This MCP
        // server is single-tenant and process-lifetime scoped, so disable idle
        // eviction entirely rather than tuning the timeout.
        // (`SessionConfig`/`LocalSessionManager` are `#[non_exhaustive]`, so
        // they must be built via `Default::default()` plus field assignment
        // rather than struct-literal syntax.)
        let mut session_manager = LocalSessionManager::default();
        session_manager.session_config.keep_alive = None;
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(session_manager),
            server_config,
        );
        let protected = axum::Router::new().nest_service(MCP_PATH, service).layer(
            axum::middleware::from_fn_with_state(authorization.clone(), require_bearer),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind subagent MCP listener")?;
        let addr = listener
            .local_addr()
            .context("read subagent MCP listener address")?;
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, protected)
                .with_graceful_shutdown(task_cancellation.cancelled_owned())
                .await
            {
                tracing::warn!("subagent MCP listener stopped: {error}");
            }
        });
        let advertised = McpServer::Http(
            McpServerHttp::new(MCP_SERVER_NAME, format!("http://{addr}{MCP_PATH}"))
                .headers(vec![HttpHeader::new("Authorization", authorization)]),
        );
        Ok(Self {
            advertised,
            cancellation,
            task,
        })
    }

    pub fn advertised(&self) -> &McpServer {
        &self.advertised
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
    }
}

async fn require_bearer(
    State(expected): State<String>,
    request: Request,
    next: Next,
) -> std::result::Result<Response, (StatusCode, &'static str)> {
    let authorized = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.as_bytes() == expected.as_bytes());
    if authorized {
        Ok(next.run(request).await)
    } else {
        Err((StatusCode::UNAUTHORIZED, "unauthorized"))
    }
}

// ---------------------------------------------------------------------------
// Controller: one shared pool
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum ActiveRun {
    Starting {
        cancel_requested: bool,
        shutdown_requested: bool,
        termination: RunTermination,
        root: PathBuf,
        overlap: Arc<AtomicUsize>,
    },
    Running {
        commands: mpsc::UnboundedSender<UiCommand>,
        termination: RunTermination,
        root: PathBuf,
        overlap: Arc<AtomicUsize>,
    },
    /// Finished but kept warm so `resume` can continue its ACP session. Idle:
    /// it holds no pool slot and does not count as an active worker.
    Retained {
        commands: mpsc::UnboundedSender<UiCommand>,
        termination: RunTermination,
        root: PathBuf,
        overlap: Arc<AtomicUsize>,
    },
}

impl ActiveRun {
    fn termination(&self) -> RunTermination {
        match self {
            Self::Starting { termination, .. }
            | Self::Running { termination, .. }
            | Self::Retained { termination, .. } => termination.clone(),
        }
    }

    fn root(&self) -> &Path {
        match self {
            Self::Starting { root, .. }
            | Self::Running { root, .. }
            | Self::Retained { root, .. } => root,
        }
    }

    fn overlap(&self) -> Arc<AtomicUsize> {
        match self {
            Self::Starting { overlap, .. }
            | Self::Running { overlap, .. }
            | Self::Retained { overlap, .. } => overlap.clone(),
        }
    }

    /// Retained runs are idle: no turn in flight and no file mutation. They
    /// must not occupy a pool slot or hold the active-worker gate open.
    fn occupies_slot(&self) -> bool {
        !matches!(self, Self::Retained { .. })
    }
}

/// One admitted subagent run: its id, its termination handle (available before
/// any follow-up await, so a slot can never be orphaned), and the counter of
/// concurrent runs that shared its workspace root.
#[derive(Debug, Clone)]
struct Admission {
    subagent_id: u64,
    termination: RunTermination,
    overlap: Arc<AtomicUsize>,
}

/// Rejection when the shared pool is at capacity. Nothing is queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolFull {
    active: Vec<u64>,
    capacity: usize,
}

impl PoolFull {
    fn message(&self) -> String {
        let active = self
            .active
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "the subagent pool is full: {} of {} slots are in use by {active}. Nothing was queued and no subagent was started. Wait for one of those reports to arrive, or stop one with subagent_cancel, then try again.",
            self.active.len(),
            self.capacity,
        )
    }
}

/// Monotonic source of subagent ids. Shared between the subagent pool and the
/// discrete-review lanes, which are not pool members but still render as
/// subagent status rows: one allocator is what keeps their ids from colliding.
#[derive(Debug, Clone)]
pub struct SubagentIdAllocator(Arc<AtomicU64>);

impl Default for SubagentIdAllocator {
    fn default() -> Self {
        Self(Arc::new(AtomicU64::new(1)))
    }
}

impl SubagentIdAllocator {
    /// Next unused id. Ids are handed out in spawn order and never reused.
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::AcqRel)
    }
}

#[derive(Debug)]
struct ControllerState {
    next_id: SubagentIdAllocator,
    max_parallel: usize,
    runs: HashMap<u64, ActiveRun>,
    active_workers: ActiveSubagentWorkers,
    active_runs: watch::Sender<usize>,
}

impl Default for ControllerState {
    fn default() -> Self {
        let (active_runs, _) = watch::channel(0);
        Self {
            next_id: SubagentIdAllocator::default(),
            max_parallel: DEFAULT_MAX_PARALLEL,
            runs: HashMap::new(),
            active_workers: ActiveSubagentWorkers::default(),
            active_runs,
        }
    }
}

/// Coordinates one shared pool of equally capable subagents.
#[derive(Debug, Clone, Default)]
pub struct Controller {
    state: Arc<Mutex<ControllerState>>,
}

impl Controller {
    async fn configure(
        &self,
        max_parallel: usize,
        active_workers: ActiveSubagentWorkers,
        id_allocator: SubagentIdAllocator,
    ) {
        let mut state = self.state.lock().await;
        state.max_parallel = max_parallel.clamp(1, MAX_PARALLEL_CAP);
        state.active_workers = active_workers;
        state.next_id = id_allocator;
    }

    /// Admits one run against the shared pool, atomically returning its
    /// termination handle so a caller never leaves an admitted-but-unclaimed
    /// slot across an await point.
    async fn begin(&self, root: PathBuf) -> std::result::Result<Admission, PoolFull> {
        let mut state = self.state.lock().await;
        if let Some(full) = state.pool_full(None) {
            return Err(full);
        }
        let overlap = Arc::new(AtomicUsize::new(0));
        for run in state.runs.values() {
            if run.occupies_slot() && run.root() == root {
                overlap.fetch_add(1, Ordering::AcqRel);
                run.overlap().fetch_add(1, Ordering::AcqRel);
            }
        }
        let subagent_id = state.next_id.next();
        state.runs.insert(
            subagent_id,
            ActiveRun::Starting {
                cancel_requested: false,
                shutdown_requested: false,
                termination: RunTermination::default(),
                root,
                overlap: overlap.clone(),
            },
        );
        let termination = state
            .runs
            .get(&subagent_id)
            .expect("newly admitted run is retained by the controller")
            .termination();
        state.refresh_active_workers();
        let active = state.runs.len();
        state.active_runs.send_replace(active);
        Ok(Admission {
            subagent_id,
            termination,
            overlap,
        })
    }

    async fn attach(&self, id: u64, commands: mpsc::UnboundedSender<UiCommand>) {
        let mut state = self.state.lock().await;
        let Some(run) = state.runs.remove(&id) else {
            let _ = commands.send(UiCommand::Shutdown);
            return;
        };
        let ActiveRun::Starting {
            cancel_requested,
            shutdown_requested,
            termination,
            root,
            overlap,
        } = run
        else {
            state.runs.insert(id, run);
            return;
        };
        state.runs.insert(
            id,
            ActiveRun::Running {
                commands: commands.clone(),
                termination,
                root,
                overlap,
            },
        );
        if shutdown_requested {
            let _ = commands.send(UiCommand::Shutdown);
        } else if cancel_requested {
            let _ = commands.send(UiCommand::CancelPrompt);
        }
    }

    pub async fn cancel(&self) -> bool {
        let mut state = self.state.lock().await;
        let mut active = false;
        for run in state.runs.values_mut() {
            active = true;
            match run {
                ActiveRun::Starting {
                    cancel_requested,
                    termination,
                    ..
                } => {
                    *cancel_requested = true;
                    termination.request(TerminationCause::UserCancelled);
                }
                ActiveRun::Running {
                    commands,
                    termination,
                    ..
                }
                | ActiveRun::Retained {
                    commands,
                    termination,
                    ..
                } => {
                    let _ = commands.send(UiCommand::CancelPrompt);
                    termination.request(TerminationCause::UserCancelled);
                }
            }
        }
        active
    }

    pub async fn shutdown(&self) -> bool {
        let mut state = self.state.lock().await;
        let mut active = false;
        for run in state.runs.values_mut() {
            active = true;
            match run {
                ActiveRun::Starting {
                    shutdown_requested,
                    termination,
                    ..
                } => {
                    *shutdown_requested = true;
                    termination.request(TerminationCause::RuntimeShutdown);
                }
                ActiveRun::Running {
                    commands,
                    termination,
                    ..
                }
                | ActiveRun::Retained {
                    commands,
                    termination,
                    ..
                } => {
                    let _ = commands.send(UiCommand::Shutdown);
                    termination.request(TerminationCause::RuntimeShutdown);
                }
            }
        }
        active
    }

    pub async fn shutdown_and_wait(&self) -> bool {
        let mut active_runs = self.state.lock().await.active_runs.subscribe();
        let active = self.shutdown().await;
        while *active_runs.borrow_and_update() > 0 {
            if active_runs.changed().await.is_err() {
                break;
            }
        }
        active
    }

    async fn cancel_and_wait(&self) -> bool {
        let mut active_runs = self.state.lock().await.active_runs.subscribe();
        let active = self.cancel().await;
        while *active_runs.borrow_and_update() > 0 {
            if active_runs.changed().await.is_err() {
                break;
            }
        }
        active
    }

    async fn retain_complete(&self, id: u64) {
        let mut state = self.state.lock().await;
        let Some(run) = state.runs.remove(&id) else {
            return;
        };
        let ActiveRun::Running {
            commands,
            termination,
            root,
            overlap,
        } = run
        else {
            state.runs.insert(id, run);
            return;
        };
        state.runs.insert(
            id,
            ActiveRun::Retained {
                commands,
                termination,
                root,
                overlap,
            },
        );
        state.refresh_active_workers();
        state.active_runs.send_replace(state.runs.len());
    }

    /// Re-admits a retained run against the shared pool for a `resume`.
    async fn resume_retained(&self, id: u64) -> std::result::Result<(), PoolFull> {
        let mut state = self.state.lock().await;
        if let Some(full) = state.pool_full(Some(id)) {
            return Err(full);
        }
        let Some(run) = state.runs.remove(&id) else {
            return Ok(());
        };
        let ActiveRun::Retained {
            commands,
            termination,
            root,
            overlap,
        } = run
        else {
            state.runs.insert(id, run);
            return Ok(());
        };
        for other in state.runs.values() {
            if other.occupies_slot() && other.root() == root {
                overlap.fetch_add(1, Ordering::AcqRel);
                other.overlap().fetch_add(1, Ordering::AcqRel);
            }
        }
        state.runs.insert(
            id,
            ActiveRun::Running {
                commands,
                termination,
                root,
                overlap,
            },
        );
        state.refresh_active_workers();
        state.active_runs.send_replace(state.runs.len());
        Ok(())
    }

    #[cfg(test)]
    async fn termination(&self, id: u64) -> Option<RunTermination> {
        self.state
            .lock()
            .await
            .runs
            .get(&id)
            .map(ActiveRun::termination)
    }

    #[cfg(test)]
    async fn wait_until_absent(&self, id: u64) {
        let mut active_runs = self.state.lock().await.active_runs.subscribe();
        loop {
            if !self.state.lock().await.runs.contains_key(&id) {
                return;
            }
            if active_runs.changed().await.is_err() {
                return;
            }
        }
    }

    async fn finish(&self, id: u64) {
        let mut state = self.state.lock().await;
        state.runs.remove(&id);
        state.refresh_active_workers();
        let active = state.runs.len();
        state.active_runs.send_replace(active);
    }

    #[cfg(test)]
    async fn active_count(&self) -> usize {
        self.state
            .lock()
            .await
            .runs
            .values()
            .filter(|run| run.occupies_slot())
            .count()
    }
}

/// Programmatic entry point for Mjolnir-owned agent coordinators.
///
/// It deliberately reuses the same controller, worker, report, cancellation,
/// and UI-event path as the public MCP tools without exposing those tools to
/// the nested runtime itself.
#[derive(Clone)]
pub(crate) struct ProgrammaticPool {
    config: Config,
    context: RunContext,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    controller: Controller,
    runs: SubagentRegistry,
}

impl ProgrammaticPool {
    pub(crate) async fn start(
        config: Config,
        context: RunContext,
        ui_tx: mpsc::UnboundedSender<UiEvent>,
    ) -> Self {
        let controller = Controller::default();
        controller
            .configure(
                config.max_parallel,
                config.active_implementation_workers.clone(),
                config.id_allocator.clone(),
            )
            .await;
        Self {
            config,
            context,
            ui_tx,
            controller,
            runs: SubagentRegistry::default(),
        }
    }

    /// Admit and start one fixed job, returning as soon as its worker exists.
    pub(crate) async fn launch(&self, job: ProgrammaticJob) -> Result<ProgrammaticStarted> {
        if job.prompt.trim().is_empty() {
            bail!("programmatic agent prompt must not be empty");
        }
        let spec = self
            .config
            .resolve_session(None, None)
            .map_err(|error| anyhow!(error.to_string()))?;
        let policy = RunPolicy::programmatic(&self.config, &job);
        let subagent_id = admit_and_launch_run(
            &self.controller,
            &self.runs,
            &self.config,
            self.context.clone(),
            job.prompt,
            job.images,
            job.label,
            spec.clone(),
            policy,
            &self.ui_tx,
        )
        .await
        .map_err(|full| anyhow!(full.message()))?;
        Ok(ProgrammaticStarted {
            subagent_id,
            agent: spec.agent,
            model: spec.model,
        })
    }

    /// Continue one retained job on its existing ACP session.
    pub(crate) async fn resume(
        &self,
        subagent_id: u64,
        prompt: String,
    ) -> Result<ProgrammaticStarted> {
        if prompt.trim().is_empty() {
            bail!("programmatic agent continuation must not be empty");
        }
        resume_retained_run(
            &self.controller,
            &self.runs,
            &self.config,
            subagent_id,
            prompt,
        )
        .await
        .map_err(|failure| anyhow!(failure.message(subagent_id)))?;
        Ok(ProgrammaticStarted {
            subagent_id,
            agent: self.config.current_agent(),
            model: self.config.current_model(),
        })
    }

    pub(crate) async fn shutdown_and_wait(&self) -> bool {
        self.controller.shutdown_and_wait().await
    }

    /// User-visible cancellation: unlike shutdown, terminal rows are labelled
    /// cancelled rather than failed.
    pub(crate) async fn cancel_and_wait(&self) -> bool {
        self.controller.cancel_and_wait().await
    }
}

impl ControllerState {
    fn pool_full(&self, exclude: Option<u64>) -> Option<PoolFull> {
        let mut active = self
            .runs
            .iter()
            .filter(|(id, run)| Some(**id) != exclude && run.occupies_slot())
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        if active.len() < self.max_parallel {
            return None;
        }
        active.sort_unstable();
        Some(PoolFull {
            active,
            capacity: self.max_parallel,
        })
    }

    fn refresh_active_workers(&self) {
        let active = self.runs.values().filter(|run| run.occupies_slot()).count();
        self.active_workers.set(active);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum TerminationCause {
    None = 0,
    UserCancelled = 1,
    RuntimeShutdown = 2,
    RunCompleted = 4,
}

impl TerminationCause {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::UserCancelled,
            2 => Self::RuntimeShutdown,
            4 => Self::RunCompleted,
            _ => Self::None,
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::None => "unspecified",
            Self::UserCancelled => "user cancellation",
            Self::RuntimeShutdown => "runtime shutdown",
            Self::RunCompleted => "normal completion",
        }
    }
}

#[derive(Clone, Debug)]
struct RunTermination {
    token: CancellationToken,
    cause: Arc<AtomicU8>,
}

impl Default for RunTermination {
    fn default() -> Self {
        Self {
            token: CancellationToken::new(),
            cause: Arc::new(AtomicU8::new(TerminationCause::None as u8)),
        }
    }
}

impl RunTermination {
    fn request(&self, cause: TerminationCause) {
        let _ = self.cause.compare_exchange(
            TerminationCause::None as u8,
            cause as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.token.cancel();
    }

    fn cause(&self) -> TerminationCause {
        TerminationCause::from_u8(self.cause.load(Ordering::Acquire))
    }

    async fn cancelled(&self) {
        self.token.cancelled().await;
    }
}

struct AgentMessageCollector {
    last: String,
    message_open: bool,
}

impl AgentMessageCollector {
    fn new() -> Self {
        Self {
            last: String::new(),
            message_open: false,
        }
    }

    fn observe(&mut self, update: &SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if !self.message_open {
                    self.last.clear();
                    self.message_open = true;
                }
                self.last.push_str(&content_block_text(&chunk.content));
            }
            SessionUpdate::UserMessageChunk(_)
            | SessionUpdate::AgentThoughtChunk(_)
            | SessionUpdate::ToolCall(_)
            | SessionUpdate::Plan(_) => self.message_open = false,
            _ => {}
        }
    }

    fn finish(&self) -> Result<String> {
        if self.last.trim().is_empty() {
            bail!("the subagent finished without a final message");
        }
        Ok(self.last.clone())
    }
}

/// Distilled one-liner for the live status of any subagent.
fn exploration_activity(update: &SessionUpdate) -> Option<String> {
    match update {
        SessionUpdate::ToolCall(call) => Some(call.title.clone()),
        SessionUpdate::ToolCallUpdate(update) => update.fields.title.clone().or_else(|| {
            update
                .fields
                .status
                .map(|status| format!("tool {status:?}"))
        }),
        SessionUpdate::Plan(_) => Some("planning".to_string()),
        _ => None,
    }
}

/// The result a `subagent_cancel` call hands back. Ordinary completions travel
/// as `SubagentReport`s instead.
struct SubagentRunResult {
    outcome: Result<String>,
    workspace_delta: Option<WorkspaceDelta>,
    activity_log: String,
    /// True only when the cancel interrupted a genuinely in-flight turn.
    /// Determined by the worker at the moment it processes the request, not by
    /// the MCP layer's registry snapshot at dispatch time.
    cancelled_while_running: bool,
}

/// Sent by `create_subagent`(resume) / `subagent_cancel`, and by retention
/// reaping, to a run's persistent worker task.
enum WorkerRequest {
    /// Continue a retained (finished, idle) session with a new prompt.
    Continue { prompt: String },
    /// Stop the run. Against a running turn this is the only interruption: the
    /// worker cancels it, lets it settle, and reports a catch-up result.
    /// Against a retained run it just releases the idle session. Neither
    /// reverts workspace edits.
    Cancel {
        respond: oneshot::Sender<SubagentRunResult>,
    },
    /// Reap an idle retained worker because retention is over capacity.
    Supersede,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubagentRunState {
    Running,
    Retained,
}

#[derive(Clone)]
struct RegisteredRun {
    state: SubagentRunState,
    control: mpsc::UnboundedSender<WorkerRequest>,
}

/// Routes `resume`/`subagent_cancel` to a run's worker, and bounds how many
/// finished sessions stay warm.
#[derive(Clone, Default)]
struct SubagentRegistry {
    runs: Arc<StdMutex<HashMap<u64, RegisteredRun>>>,
    /// Insertion order of retained runs, oldest first, so retention reaping is
    /// deterministic.
    retained_order: Arc<StdMutex<Vec<u64>>>,
}

impl SubagentRegistry {
    fn insert_running(&self, subagent_id: u64, control: mpsc::UnboundedSender<WorkerRequest>) {
        self.lock_runs().insert(
            subagent_id,
            RegisteredRun {
                state: SubagentRunState::Running,
                control,
            },
        );
        self.lock_order().retain(|id| *id != subagent_id);
    }

    /// Marks a run retained and returns whichever oldest retained runs now
    /// exceed `retain_limit` so the caller can reap them.
    fn insert_retained(
        &self,
        subagent_id: u64,
        control: mpsc::UnboundedSender<WorkerRequest>,
        retain_limit: usize,
    ) -> Vec<mpsc::UnboundedSender<WorkerRequest>> {
        let mut runs = self.lock_runs();
        runs.insert(
            subagent_id,
            RegisteredRun {
                state: SubagentRunState::Retained,
                control,
            },
        );
        let mut order = self.lock_order();
        order.retain(|id| *id != subagent_id);
        order.push(subagent_id);
        let mut reaped = Vec::new();
        while order.len() > retain_limit.max(1) {
            let oldest = order.remove(0);
            if let Some(run) = runs.remove(&oldest) {
                reaped.push(run.control);
            }
        }
        reaped
    }

    /// Puts a retained run back after a rejected resume, without disturbing the
    /// retention order more than necessary.
    fn reinstate_retained(
        &self,
        subagent_id: u64,
        control: mpsc::UnboundedSender<WorkerRequest>,
        retain_limit: usize,
    ) {
        for reaped in self.insert_retained(subagent_id, control, retain_limit) {
            let _ = reaped.send(WorkerRequest::Supersede);
        }
    }

    /// Atomically removes and returns the control sender for a run, so at most
    /// one in-flight resume/cancel request can act on it at a time.
    fn take(&self, subagent_id: u64) -> Option<RegisteredRun> {
        self.lock_order().retain(|id| *id != subagent_id);
        self.lock_runs().remove(&subagent_id)
    }

    #[cfg(test)]
    fn retained_ids(&self) -> Vec<u64> {
        self.lock_order().clone()
    }

    fn lock_runs(&self) -> std::sync::MutexGuard<'_, HashMap<u64, RegisteredRun>> {
        self.runs.lock().expect("subagent registry lock poisoned")
    }

    fn lock_order(&self) -> std::sync::MutexGuard<'_, Vec<u64>> {
        self.retained_order
            .lock()
            .expect("subagent retention order lock poisoned")
    }
}

fn unresolved_subagent_message(subagent_id: u64) -> String {
    format!(
        "subagent_id {subagent_id} is not a known subagent; it may never have existed, or it was already released by an earlier cancel or reaped once retention filled up"
    )
}

fn still_running_message(subagent_id: u64) -> String {
    format!(
        "subagent #{subagent_id} is still running, so it cannot be resumed. Its report will arrive on its own when it finishes; resume it then, or stop it with subagent_cancel."
    )
}

fn worker_unavailable_message(subagent_id: u64) -> String {
    format!(
        "subagent #{subagent_id} is no longer available; its worker ended unexpectedly. Any partial edits it made remain in the workspace; start a new subagent if needed."
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ResumeFailure {
    Unknown,
    Running,
    PoolFull(PoolFull),
    WorkerUnavailable,
}

impl ResumeFailure {
    fn message(&self, subagent_id: u64) -> String {
        match self {
            Self::Unknown => unresolved_subagent_message(subagent_id),
            Self::Running => still_running_message(subagent_id),
            Self::PoolFull(full) => full.message(),
            Self::WorkerUnavailable => worker_unavailable_message(subagent_id),
        }
    }
}

fn continuation_prompt(guidance: &str) -> String {
    format!(
        "Continuing your earlier task in the same session; your previous progress is preserved in the workspace.\n\n{guidance}"
    )
}

/// Shared admission path for public MCP and Mjolnir-owned programmatic runs.
///
/// In particular, report accounting opens before the worker can finish, and
/// the registry entry is installed before the task is spawned.
#[allow(clippy::too_many_arguments)]
async fn admit_and_launch_run(
    controller: &Controller,
    registry: &SubagentRegistry,
    config: &Config,
    context: RunContext,
    task: String,
    images: Vec<PromptImage>,
    label: String,
    spec: SessionSpec,
    policy: RunPolicy,
    ui_tx: &mpsc::UnboundedSender<UiEvent>,
) -> std::result::Result<u64, PoolFull> {
    let root = canonical_root(&context.cwd).await;
    let admission = controller.begin(root).await?;
    let subagent_id = admission.subagent_id;
    if let Some(reports) = config.reports.as_ref() {
        reports.open();
    }
    launch_subagent_worker(
        controller.clone(),
        registry.clone(),
        config.clone(),
        context,
        task,
        images,
        label,
        spec,
        policy,
        ui_tx.clone(),
        admission,
    );
    Ok(subagent_id)
}

/// Shared retained-session handoff for public MCP and programmatic callers.
///
/// The controller and registry become running before the continuation is sent;
/// a failed send rolls both back together with the report counter.
async fn resume_retained_run(
    controller: &Controller,
    registry: &SubagentRegistry,
    config: &Config,
    subagent_id: u64,
    prompt: String,
) -> std::result::Result<(), ResumeFailure> {
    let Some(run) = registry.take(subagent_id) else {
        return Err(ResumeFailure::Unknown);
    };
    if run.state == SubagentRunState::Running {
        registry.insert_running(subagent_id, run.control);
        return Err(ResumeFailure::Running);
    }
    if let Err(full) = controller.resume_retained(subagent_id).await {
        registry.reinstate_retained(subagent_id, run.control, config.max_parallel);
        return Err(ResumeFailure::PoolFull(full));
    }
    // Register before handing the worker the prompt: the worker can finish and
    // mark itself retained on the very next poll.
    registry.insert_running(subagent_id, run.control.clone());
    if let Some(reports) = config.reports.as_ref() {
        reports.open();
    }
    if run
        .control
        .send(WorkerRequest::Continue { prompt })
        .is_err()
    {
        registry.take(subagent_id);
        controller.finish(subagent_id).await;
        if let Some(reports) = config.reports.as_ref() {
            reports.close();
        }
        return Err(ResumeFailure::WorkerUnavailable);
    }
    Ok(())
}

/// `result.cancelled_while_running` distinguishes a cancel that interrupted a
/// genuinely in-flight turn from releasing an idle retained run. The worker
/// sets that field itself at the moment it processes the cancel, so it stays
/// correct even if the cancel crosses in flight with the run finishing.
fn cancelled_tool_result(result: &SubagentRunResult) -> CallToolResult {
    let message = if result.cancelled_while_running {
        "The subagent was cancelled while still working. It did not revert any changes: its edits remain in the workspace exactly as it left them. No report will be injected for it. Activity so far:"
    } else if result.outcome.is_ok() {
        "The subagent's retained session was released. It did not revert any changes: its edits remain in the workspace exactly as it left them."
    } else {
        "The subagent was cancelled before finishing. It did not revert any changes: partial edits remain in the workspace exactly as it left them."
    };
    CallToolResult::success(vec![Content::text(with_workspace_diff(
        message,
        &result.activity_log,
        result.workspace_delta.as_ref(),
    ))])
}

/// Spawns the persistent worker and registers it. The tool call returns without
/// waiting for any of it.
#[allow(clippy::too_many_arguments)]
fn launch_subagent_worker(
    controller: Controller,
    registry: SubagentRegistry,
    config: Config,
    context: RunContext,
    task: String,
    images: Vec<PromptImage>,
    label: String,
    spec: SessionSpec,
    policy: RunPolicy,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    admission: Admission,
) -> mpsc::UnboundedSender<WorkerRequest> {
    let (control_tx, control_rx) = mpsc::unbounded_channel();
    let subagent_id = admission.subagent_id;
    // Register before spawning: the worker can reach its retained state on the
    // very next poll, and must not be overwritten by a late `insert_running`.
    registry.insert_running(subagent_id, control_tx.clone());
    let worker = run_boxed(
        config,
        context,
        task,
        images,
        label,
        spec,
        policy,
        ui_tx,
        RunLease {
            controller: controller.clone(),
            registry,
            subagent_id,
            termination: admission.termination,
            overlap: admission.overlap,
            control_tx: control_tx.clone(),
        },
        control_rx,
    );
    launch_subagent_worker_task(controller, subagent_id, worker);
    control_tx
}

/// Owns the worker independently of MCP request futures: `create_subagent`
/// returns immediately, so nothing else keeps it alive. This task releases the
/// controller slot only once the worker has truly finished.
fn launch_subagent_worker_task<F>(controller: Controller, subagent_id: u64, worker: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let worker = tokio::spawn(worker);
        if let Err(error) = worker.await {
            tracing::error!(
                event = "subagent_worker_task_failed",
                subagent_id,
                error = %error,
                "subagent worker task ended unexpectedly"
            );
        }
        controller.finish(subagent_id).await;
        tracing::info!(
            event = "subagent_slot_released",
            subagent_id,
            "subagent controller slot released after reap"
        );
    });
}

struct RunLease {
    controller: Controller,
    registry: SubagentRegistry,
    subagent_id: u64,
    termination: RunTermination,
    overlap: Arc<AtomicUsize>,
    control_tx: mpsc::UnboundedSender<WorkerRequest>,
}

#[allow(clippy::too_many_arguments)]
fn run_boxed(
    config: Config,
    context: RunContext,
    task: String,
    images: Vec<PromptImage>,
    label: String,
    spec: SessionSpec,
    policy: RunPolicy,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    lease: RunLease,
    control_rx: mpsc::UnboundedReceiver<WorkerRequest>,
) -> futures::future::BoxFuture<'static, ()> {
    Box::pin(run(
        config, context, task, images, label, spec, policy, ui_tx, lease, control_rx,
    ))
}

/// Maps a `RunTermination` cause to the error `run()` reports for it.
fn termination_error(cause: TerminationCause) -> anyhow::Error {
    match cause {
        TerminationCause::UserCancelled => anyhow!("the subagent was cancelled"),
        TerminationCause::RuntimeShutdown => anyhow!("subagent shutdown requested"),
        TerminationCause::RunCompleted | TerminationCause::None => {
            anyhow!("subagent termination requested")
        }
    }
}

/// Resolve termination while the worker is idle after a successful retained
/// turn. Runtime shutdown is normal lifecycle completion in this state, not an
/// agent failure; user cancellation remains distinguishable in the UI and
/// telemetry.
fn retained_termination_result(cause: TerminationCause) -> Result<String> {
    match cause {
        TerminationCause::RuntimeShutdown => {
            Ok("the completed retained subagent session was shut down".to_string())
        }
        _ => Err(termination_error(cause)),
    }
}

/// Maps the nested ACP runtime's join outcome to (a) the raw result recorded
/// for teardown-failure logging and (b) the run-level error it implies.
fn map_runtime_join(
    joined: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> (Result<()>, Result<String>) {
    match joined {
        Ok(Ok(())) => (
            Ok(()),
            Err(anyhow!("the subagent runtime closed before completing")),
        ),
        Ok(Err(error)) => {
            let message = format!("{error:#}");
            (Err(error), Err(anyhow!("subagent runtime: {message}")))
        }
        Err(error) => {
            let message = format!("subagent task failed: {error}");
            (Err(anyhow!(message.clone())), Err(anyhow!(message)))
        }
    }
}

fn outcome_for(result: &Result<String>) -> SubagentOutcome {
    match result {
        Ok(_) => SubagentOutcome::Completed,
        Err(error) if error.to_string().contains("cancel") => SubagentOutcome::Cancelled,
        Err(error) => SubagentOutcome::Failed(error.to_string()),
    }
}

/// Renders the workspace section of a report: the per-run diff, or the note
/// explaining that concurrent subagents made an attributable diff impossible.
fn report_workspace_diff(delta: Option<&WorkspaceDelta>, overlap: usize) -> Option<String> {
    if overlap > 0 {
        return Some(format!(
            "omitted: {overlap} subagent{} shared this workspace during the run — inspect git diff yourself",
            if overlap == 1 { "" } else { "s" }
        ));
    }
    let delta = delta?;
    Some(
        delta
            .review_patch()
            .map(str::to_string)
            .unwrap_or_else(|| delta.receipt().to_string()),
    )
}

/// Runs one subagent end to end. The tool call that started it has already
/// returned, so every result leaves through the report bus (ordinary
/// completions) or a `Cancel` responder (caller-initiated cancels). After each
/// successful turn the ACP session is retained idle so `resume` can continue it.
#[allow(clippy::too_many_arguments)]
async fn run(
    mut config: Config,
    context: RunContext,
    task: String,
    images: Vec<PromptImage>,
    label: String,
    spec: SessionSpec,
    policy: RunPolicy,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    lease: RunLease,
    mut control_rx: mpsc::UnboundedReceiver<WorkerRequest>,
) {
    let RunLease {
        controller,
        registry,
        subagent_id,
        termination,
        overlap,
        control_tx,
    } = lease;
    let mut cancel_respond: Option<oneshot::Sender<SubagentRunResult>> = None;
    if let Some(workflow) = policy.workflow.as_ref() {
        workflow.started(subagent_id);
    }
    let use_warm = spec.role.is_none() && policy.allow_warm_runtime;
    let mut quota_role = None;
    match spec.role.clone() {
        Some(role) => config.apply_role(*role),
        None => {
            if let Some(pool) = config.role_pool.clone() {
                match pool.select_for_work().await {
                    Ok(selection) => {
                        quota_role = Some(selection.role.clone());
                        config.apply_role(selection.role);
                    }
                    Err(message) => {
                        deliver_report(
                            &config,
                            SubagentReport {
                                subagent_id,
                                label: label.clone(),
                                agent: spec.agent.clone(),
                                model: spec.model.clone(),
                                outcome: SubagentOutcome::Failed(message.clone()),
                                final_message: format!(
                                    "{message}. The subagent was not started; decide how to proceed yourself."
                                ),
                                slim_activity: render_activity_log(&[]),
                                workspace_diff: None,
                                elapsed: Duration::ZERO,
                            },
                        );
                        let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::Finished {
                            subagent_id,
                            outcome: SubagentOutcome::Failed(message.clone()),
                        }));
                        if let Some(workflow) = policy.workflow.as_ref() {
                            workflow.finished(subagent_id, SubagentOutcome::Failed(message));
                        }
                        return;
                    }
                }
            }
        }
    }
    let agent_id = config.current_agent();
    let model_id = config.current_model();
    let log_role = config.role_config.clone();
    tracing::info!(
        event = "subagent_worker_started",
        subagent_id,
        agent = %agent_id,
        model = %model_id,
        "subagent worker started"
    );
    if let Some(role) = log_role.as_ref()
        && let Some(session_tag) = role.session_tag.as_deref()
    {
        tracing::info!(
            event = "subagent_started",
            session_tag,
            model = %role.model_id,
            adapter = %role.adapter_source_id,
            subagent_id,
            task = %task,
            "the primary agent launched a subagent"
        );
    }
    let _ = ui_tx.send(UiEvent::InternalMessage(InternalMessage {
        source: "primary".to_string(),
        target: format!("subagent #{subagent_id}"),
        kind: InternalMessageKind::Delegation,
        text: task.clone(),
        owner_subagent_id: Some(subagent_id),
    }));
    let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::Started {
        subagent_id,
        resumed: false,
        label: label.clone(),
        model: Some(model_id.clone()),
        agent: agent_id.clone(),
        objective: label.clone(),
    }));

    let warm = use_warm.then(|| config.take_warm(&context)).flatten();
    let WarmRuntime {
        events: mut nested_event_rx,
        commands: nested_cmd_tx,
        task: mut runtime,
        cancel: runtime_cancel,
        ..
    } = warm.unwrap_or_else(|| {
        spawn_subagent_runtime(
            &config,
            context.clone(),
            Some(termination.token.clone()),
            &policy.mcp_servers,
        )
    });
    if use_warm {
        config.ensure_warm(context.clone());
    }
    controller.attach(subagent_id, nested_cmd_tx.clone()).await;

    let mut awaiting_session_start = true;
    let mut prompt_to_send = Some((format!("{}{task}", policy.preamble), images));
    let mut tracker = BoundaryTracker::default();
    let mut latest_usage_update: Option<UsageUpdate> = None;
    let mut session_id = None;
    let mut joined_runtime_result = None;
    let mut activity = SubagentTranscript::default();
    // Entry count in `activity` as of the last report delivered; a cancel that
    // interrupts a running turn reports only the tail past this mark.
    let mut watermark: usize = 0;
    let mut cancelled_while_running = false;
    let mut turn_started = Instant::now();
    let mut invocation_snapshot: Option<WorkspaceSnapshot> = None;
    // Every admitted turn owes exactly one report, so the orchestrator's
    // outstanding-report accounting (which headless drains on) always balances
    // even when the turn ends through external termination instead of on its
    // own.
    let mut turn_reported = false;
    // A retained programmatic coordinator remains one live UI identity between
    // turns. Its terminal event is deferred until the worker itself is reaped.
    let mut terminal_finished_pending = false;

    let mut result: Result<String> = 'session: loop {
        if invocation_snapshot.is_none() {
            invocation_snapshot = Some(capture_workspace_snapshot(&context).await);
            turn_started = Instant::now();
            turn_reported = false;
        }
        let mut collector = AgentMessageCollector::new();
        // Distinguishes our own `subagent_cancel`-triggered CancelPrompt
        // settling from an external cancellation reaching the same
        // `StopReason::Cancelled` event.
        let mut awaiting_cancel_settle = false;
        let mut tool_lifecycle = PromptToolLifecycle::default();
        let mut deferred_completion = None;

        let turn_result: Result<String> = 'turn: loop {
            tokio::select! {
                biased;
                () = termination.cancelled() => {
                    break 'turn Err(termination_error(termination.cause()));
                }
                request = control_rx.recv() => {
                    match request {
                        Some(WorkerRequest::Cancel { respond }) => {
                            cancel_respond = Some(respond);
                            cancelled_while_running = true;
                            awaiting_cancel_settle = true;
                            let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::Status {
                                subagent_id,
                                kind: SubagentStatusKind::Info,
                                message: "cancellation requested; stopping the in-flight turn".to_string(),
                            }));
                            let _ = nested_cmd_tx.send(UiCommand::CancelPrompt);
                        }
                        Some(WorkerRequest::Continue { .. }) => {
                            tracing::warn!(
                                event = "subagent_unexpected_control_message",
                                subagent_id,
                                "ignoring a resume while a subagent turn is still active"
                            );
                        }
                        Some(WorkerRequest::Supersede) => {
                            tracing::warn!(
                                event = "subagent_unexpected_control_message",
                                subagent_id,
                                "ignoring a supersede while a subagent turn is still active"
                            );
                        }
                        None => {
                            break 'turn Err(anyhow!(
                                "the subagent's control channel closed unexpectedly while active"
                            ));
                        }
                    }
                }
                joined = &mut runtime => {
                    let (runtime_result, run_result) = map_runtime_join(joined);
                    joined_runtime_result = Some(runtime_result);
                    break 'turn run_result;
                }
                event = nested_event_rx.recv() => {
                    let Some(event) = event else {
                        break 'turn Err(anyhow!("the subagent's event stream closed before completing"));
                    };
                    let boundary = tracker.observe(&event);
                    activity.observe(&event, boundary.as_ref());
                    match event {
                        UiEvent::Side(_) | UiEvent::SideStartFailed { .. } => {}
                        UiEvent::Connected { .. } => {}
                        UiEvent::ContextCompacted => {}
                        UiEvent::SessionStarted { session_id: started, .. } if awaiting_session_start => {
                            if let Some(workflow) = policy.workflow.as_ref() {
                                workflow.session_bound(subagent_id, started.clone());
                            }
                            let _ = ui_tx.send(UiEvent::Subagent(
                                SubagentEvent::SessionStarted {
                                    subagent_id,
                                    session_id: started.clone(),
                                },
                            ));
                            session_id = Some(started);
                            awaiting_session_start = false;
                            if let Some((prompt, images)) = prompt_to_send.take()
                                && nested_cmd_tx
                                    .send(UiCommand::SendPrompt {
                                        text: prompt,
                                        images,
                                    })
                                    .is_err()
                            {
                                break 'turn Err(anyhow!("send the prompt to the subagent"));
                            }
                        }
                        UiEvent::SessionStarted { .. }
                        | UiEvent::SessionConfigOptions { .. }
                        | UiEvent::RosterUpdate { .. }
                        | UiEvent::Workflow(_)
                        | UiEvent::WorkspaceDiff(_) => {}
                        UiEvent::SessionUpdate(update) => {
                            tool_lifecycle.observe(&update);
                            if let SessionUpdate::UsageUpdate(value) = &update {
                                latest_usage_update = Some(value.clone());
                            }
                            collector.observe(&update);
                            if let Some(activity) = exploration_activity(&update) {
                                let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::Activity {
                                    subagent_id,
                                    activity,
                                }));
                            }
                            let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::SessionUpdate {
                                subagent_id,
                                update,
                            }));
                            if !tool_lifecycle.has_active_tools()
                                && let Some((stop_reason, usage)) = deferred_completion.take()
                            {
                                let _ = ui_tx.send(UiEvent::AgentUsage(Record {
                                    seat: policy.usage_seat,
                                    model: Some(model_id.clone()),
                                    usage,
                                    update: latest_usage_update.take(),
                                    session_id: session_id.clone(),
                                }));
                                break 'turn if matches!(stop_reason, StopReason::Cancelled) {
                                    Err(anyhow!("the subagent was cancelled"))
                                } else {
                                    collector.finish()
                                };
                            }
                        }
                        UiEvent::TerminalOutput(snapshot) => {
                            let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::TerminalOutput {
                                subagent_id,
                                snapshot,
                            }));
                        }
                        UiEvent::PermissionRequest(prompt) => {
                            let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::PermissionRequest {
                                subagent_id,
                                prompt,
                            }));
                        }
                        UiEvent::ElicitationRequest(prompt) => {
                            let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::ElicitationRequest {
                                subagent_id,
                                prompt,
                            }));
                        }
                        UiEvent::CancelPendingPermissions => {
                            let _ = ui_tx.send(UiEvent::Subagent(
                                SubagentEvent::CancelPendingPermissions { subagent_id },
                            ));
                        }
                        UiEvent::Info(message) => {
                            let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::Status {
                                subagent_id,
                                kind: SubagentStatusKind::Info,
                                message,
                            }));
                        }
                        UiEvent::Warning(message) => {
                            let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::Status {
                                subagent_id,
                                kind: SubagentStatusKind::Warning,
                                message,
                            }));
                        }
                        UiEvent::PromptDone { stop_reason, usage } => {
                            if tool_lifecycle.has_active_tools()
                                && !matches!(stop_reason, StopReason::Cancelled)
                            {
                                deferred_completion.get_or_insert((stop_reason, usage));
                                continue 'turn;
                            }
                            let _ = ui_tx.send(UiEvent::AgentUsage(Record {
                                seat: policy.usage_seat,
                                model: Some(model_id.clone()),
                                usage,
                                update: latest_usage_update.take(),
                                session_id: session_id.clone(),
                            }));
                            if matches!(stop_reason, StopReason::Cancelled) {
                                if awaiting_cancel_settle
                                    && termination.cause() == TerminationCause::None
                                {
                                    // Our own subagent_cancel-triggered
                                    // CancelPrompt settled; the run ends here
                                    // and the teardown below delivers the
                                    // catch-up result via `cancel_respond`.
                                    break 'turn Err(anyhow!(
                                        "the subagent was cancelled while still working; its edits remain in the workspace as left"
                                    ));
                                }
                                break 'turn Err(anyhow!("the subagent was cancelled"));
                            }
                            break 'turn collector.finish();
                        }
                        UiEvent::PromptFailed { message }
                        | UiEvent::SessionForkFailed { message }
                        | UiEvent::Fatal(message) => {
                            break 'turn Err(anyhow!(message));
                        }
                        UiEvent::ClaudeUsage(_)
                        | UiEvent::CodexUsage(_)
                        | UiEvent::AgentUsage(_)
                        | UiEvent::SubagentPoolModelChanged { .. }
                        | UiEvent::RemotePermissionDecision { .. }
                        | UiEvent::InternalMessage(_) => {}
                        UiEvent::Subagent(_) => {
                            break 'turn Err(anyhow!("a subagent attempted recursive delegation"));
                        }
                    }
                }
            }
        };

        // A turn that ended on its own -- no external termination, no
        // caller-initiated cancel -- produces a report and retains its session.
        if termination.cause() == TerminationCause::None && cancel_respond.is_none() {
            let delta = match invocation_snapshot.take() {
                Some(snapshot) => Some(snapshot.delta().await),
                None => None,
            };
            let slim_activity = activity.render_since(watermark);
            watermark = activity.len();
            let outcome = outcome_for(&turn_result);
            let final_message = match turn_result.as_ref() {
                Ok(message) => message.clone(),
                Err(error) => format!("{error:#}"),
            };
            let report = SubagentReport {
                subagent_id,
                label: label.clone(),
                agent: agent_id.clone(),
                model: model_id.clone(),
                outcome: outcome.clone(),
                final_message,
                slim_activity,
                workspace_diff: report_workspace_diff(
                    delta.as_ref(),
                    overlap.load(Ordering::Acquire),
                ),
                elapsed: turn_started.elapsed(),
            };
            if turn_result.is_ok() && policy.retain_after_completion {
                // Publish the report only after resume can observe the retained
                // state. A coordinator is allowed to resume as soon as it
                // receives the report, so report-before-retain is a real race.
                controller.retain_complete(subagent_id).await;
                for reaped in
                    registry.insert_retained(subagent_id, control_tx.clone(), config.max_parallel)
                {
                    let _ = reaped.send(WorkerRequest::Supersede);
                }
            }
            let remains_retained = turn_result.is_ok() && policy.retain_after_completion;
            deliver_report(&config, report);
            turn_reported = true;
            if remains_retained && policy.defer_finished_while_retained {
                terminal_finished_pending = true;
            } else {
                terminal_finished_pending = false;
                let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::Finished {
                    subagent_id,
                    outcome: outcome.clone(),
                }));
                if let Some(workflow) = policy.workflow.as_ref() {
                    workflow.finished(subagent_id, outcome);
                }
            }
            if turn_result.is_err() {
                // A failed turn leaves no session worth resuming.
                registry.take(subagent_id);
                break 'session turn_result;
            }
            if !policy.retain_after_completion {
                registry.take(subagent_id);
                break 'session turn_result;
            }
            tracing::info!(
                event = "subagent_retained",
                subagent_id,
                "subagent finished and its session was retained for resume"
            );
            let message = if policy.defer_finished_while_retained {
                "turn complete; session retained for automatic resume"
            } else {
                "finished; session retained for resume"
            };
            let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::Status {
                subagent_id,
                kind: SubagentStatusKind::Info,
                message: message.to_string(),
            }));

            let mut retained_events_open = true;
            'retained: loop {
                tokio::select! {
                biased;
                () = termination.cancelled() => {
                    break 'session retained_termination_result(termination.cause());
                }
                joined = &mut runtime => {
                    let (runtime_result, run_result) = map_runtime_join(joined);
                    joined_runtime_result = Some(runtime_result);
                    break 'session run_result;
                }
                event = nested_event_rx.recv(), if retained_events_open => {
                    // The turn is over, but the runtime can still emit late
                    // terminal snapshots (a command that outlived the turn)
                    // and trailing tool-call updates. Forward them so the
                    // transcript's tool entries don't sit at "waiting for
                    // output" forever.
                    match event {
                        Some(UiEvent::TerminalOutput(snapshot)) => {
                            let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::TerminalOutput {
                                subagent_id,
                                snapshot,
                            }));
                        }
                        Some(UiEvent::SessionUpdate(update)) => {
                            let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::SessionUpdate {
                                subagent_id,
                                update,
                            }));
                        }
                        Some(_) => {}
                        None => retained_events_open = false,
                    }
                    continue 'retained;
                }
                request = control_rx.recv() => {
                    match request {
                        Some(WorkerRequest::Continue { prompt }) => {
                            // The pool slot was already re-acquired by the
                            // resume call before it handed us this prompt.
                            if let Some(workflow) = policy.workflow.as_ref() {
                                workflow.resumed(subagent_id);
                            }
                            let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::Started {
                                subagent_id,
                                resumed: true,
                                label: label.clone(),
                                model: Some(model_id.clone()),
                                agent: agent_id.clone(),
                                objective: label.clone(),
                            }));
                            if nested_cmd_tx
                                .send(UiCommand::SendPrompt {
                                    text: continuation_prompt(&prompt),
                                    images: Vec::new(),
                                })
                                .is_err()
                            {
                                break 'session Err(anyhow!("send the resume prompt to the subagent"));
                            }
                            continue 'session;
                        }
                        Some(WorkerRequest::Cancel { respond }) => {
                            tracing::info!(
                                event = "subagent_released",
                                subagent_id,
                                "a retained subagent session was released"
                            );
                            cancel_respond = Some(respond);
                            break 'session Ok(
                                "the retained subagent session was released".to_string()
                            );
                        }
                        Some(WorkerRequest::Supersede) => {
                            tracing::info!(
                                event = "subagent_superseded",
                                subagent_id,
                                "a retained subagent session was reaped to stay within the retention limit"
                            );
                            break 'session Ok(
                                "the retained subagent session was superseded".to_string()
                            );
                        }
                        None => {
                            break 'session Err(anyhow!(
                                "the retained subagent's control channel closed"
                            ));
                        }
                    }
                }
                }
            }
        }

        break 'session turn_result;
    };

    registry.take(subagent_id);

    // Never abort `acp::run`: its tail owns process-tree termination and
    // reaping. Cancelling this token drives that tail, and the supervisor
    // retains the slot until the join returns.
    let requested_cause = termination.cause();
    termination.request(TerminationCause::RunCompleted);
    runtime_cancel.cancel();
    let _ = nested_cmd_tx.send(UiCommand::Shutdown);
    let cause = termination.cause();
    tracing::info!(
        event = "subagent_termination_requested",
        subagent_id,
        reason = cause.description(),
        "terminating the subagent process tree"
    );
    let runtime_result = match joined_runtime_result {
        Some(result) => result,
        None => match runtime.await {
            Ok(result) => result,
            Err(error) => Err(anyhow!("subagent runtime task failed: {error}")),
        },
    };
    if let Err(error) = runtime_result {
        tracing::error!(event = "subagent_teardown_failure", subagent_id, error = %error, "subagent runtime failed while terminating or reaping");
        result = Err(error.context("subagent teardown"));
    } else {
        tracing::info!(
            event = "subagent_reaped",
            subagent_id,
            "subagent process tree reaped"
        );
    }

    if terminal_finished_pending && turn_reported {
        let outcome = match requested_cause {
            TerminationCause::UserCancelled => SubagentOutcome::Cancelled,
            TerminationCause::RuntimeShutdown => SubagentOutcome::Completed,
            TerminationCause::None | TerminationCause::RunCompleted => outcome_for(&result),
        };
        let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::Finished {
            subagent_id,
            outcome: outcome.clone(),
        }));
        if let Some(workflow) = policy.workflow.as_ref() {
            workflow.finished(subagent_id, outcome);
        }
    }

    if let Some(respond) = cancel_respond {
        let workspace_delta = match invocation_snapshot {
            Some(snapshot) => Some(snapshot.delta().await),
            None => None,
        };
        let activity_log = activity.render_since(watermark);
        if cancelled_while_running {
            // A cancel that interrupted a live turn still emits a report so the
            // outstanding-report accounting balances; the orchestrator drops it
            // because this tool result already carried the whole story.
            deliver_report(
                &config,
                SubagentReport {
                    subagent_id,
                    label: label.clone(),
                    agent: agent_id.clone(),
                    model: model_id.clone(),
                    outcome: SubagentOutcome::Cancelled,
                    final_message: "cancelled by the primary agent".to_string(),
                    slim_activity: activity_log.clone(),
                    workspace_diff: report_workspace_diff(
                        workspace_delta.as_ref(),
                        overlap.load(Ordering::Acquire),
                    ),
                    elapsed: turn_started.elapsed(),
                },
            );
            let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::Finished {
                subagent_id,
                outcome: SubagentOutcome::Cancelled,
            }));
            if let Some(workflow) = policy.workflow.as_ref() {
                workflow.finished(subagent_id, SubagentOutcome::Cancelled);
            }
        }
        let _ = respond.send(SubagentRunResult {
            outcome: result,
            workspace_delta,
            activity_log,
            cancelled_while_running,
        });
        return;
    }

    if !turn_reported {
        // External termination (a user cancel, or runtime shutdown) ended the
        // turn before it could report for itself.
        let outcome = outcome_for(&result);
        let final_message = match result.as_ref() {
            Ok(message) => message.clone(),
            Err(error) => format!("{error:#}"),
        };
        let workspace_delta = match invocation_snapshot {
            Some(snapshot) => Some(snapshot.delta().await),
            None => None,
        };
        deliver_report(
            &config,
            SubagentReport {
                subagent_id,
                label: label.clone(),
                agent: agent_id.clone(),
                model: model_id.clone(),
                outcome: outcome.clone(),
                final_message,
                slim_activity: activity.render_since(watermark),
                workspace_diff: report_workspace_diff(
                    workspace_delta.as_ref(),
                    overlap.load(Ordering::Acquire),
                ),
                elapsed: turn_started.elapsed(),
            },
        );
        let _ = ui_tx.send(UiEvent::Subagent(SubagentEvent::Finished {
            subagent_id,
            outcome: outcome.clone(),
        }));
        if let Some(workflow) = policy.workflow.as_ref() {
            workflow.finished(subagent_id, outcome);
        }
    }

    if result
        .as_ref()
        .is_err_and(|error| !error.to_string().contains("cancel"))
        && let (Some(pool), Some(role)) = (config.role_pool.as_ref(), quota_role.as_ref())
    {
        pool.observe_failure(role).await;
    }
    if let Some(role) = log_role.as_ref()
        && let Some(session_tag) = role.session_tag.as_deref()
    {
        tracing::info!(
            event = "subagent_finished",
            session_tag,
            model = %role.model_id,
            adapter = %role.adapter_source_id,
            subagent_id,
            outcome = if result.is_ok() { "completed" } else { "failed" },
            error = ?result.as_ref().err().map(|error| format!("{error:#}")),
            "subagent finished"
        );
    }
}

fn deliver_report(config: &Config, report: SubagentReport) {
    match config.reports.as_ref() {
        Some(bus) => bus.deliver(report),
        None => tracing::debug!(
            event = "subagent_report_dropped",
            subagent_id = report.subagent_id,
            "no report bus is wired; the subagent report was discarded"
        ),
    }
}

#[derive(Default)]
struct SubagentTranscript {
    entries: Vec<String>,
    tools: HashMap<String, ToolActivity>,
    terminal_tools: HashMap<String, String>,
}

#[derive(Default)]
struct ToolActivity {
    title: String,
    terminal_backed: bool,
    emitted: bool,
}

impl SubagentTranscript {
    fn observe(&mut self, event: &UiEvent, checkpoint: Option<&Checkpoint>) {
        let tool_event = self.observe_tool_event(event);
        if let Some(checkpoint) = checkpoint {
            if tool_event {
                if let Some(prefix) = agent_prefix_before_tool_result(&checkpoint.text) {
                    self.push(prefix);
                }
            } else {
                self.push(checkpoint.text.trim().to_string());
            }
        }
    }

    fn observe_tool_event(&mut self, event: &UiEvent) -> bool {
        match event {
            UiEvent::SessionUpdate(SessionUpdate::ToolCall(call)) => {
                let id = call.tool_call_id.to_string();
                let entry = self.tools.entry(id.clone()).or_default();
                if !call.title.trim().is_empty() {
                    entry.title = call.title.clone();
                }
                for content in &call.content {
                    if let ToolCallContent::Terminal(terminal) = content {
                        entry.terminal_backed = true;
                        self.terminal_tools
                            .insert(terminal.terminal_id.to_string(), id.clone());
                    }
                }
                if matches!(
                    call.status,
                    ToolCallStatus::Completed | ToolCallStatus::Failed
                ) && !entry.terminal_backed
                {
                    let failed = call.status == ToolCallStatus::Failed;
                    self.push_tool(&id, failed);
                }
                true
            }
            UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(update)) => {
                let id = update.tool_call_id.to_string();
                let entry = self.tools.entry(id.clone()).or_default();
                if let Some(title) = update.fields.title.as_ref()
                    && !title.trim().is_empty()
                {
                    entry.title = title.clone();
                }
                if let Some(content) = update.fields.content.as_ref() {
                    for content in content {
                        if let ToolCallContent::Terminal(terminal) = content {
                            entry.terminal_backed = true;
                            self.terminal_tools
                                .insert(terminal.terminal_id.to_string(), id.clone());
                        }
                    }
                }
                if let Some(status @ (ToolCallStatus::Completed | ToolCallStatus::Failed)) =
                    update.fields.status
                    && !entry.terminal_backed
                {
                    self.push_tool(&id, status == ToolCallStatus::Failed);
                }
                true
            }
            UiEvent::TerminalOutput(snapshot) if snapshot.exit_status.is_some() => {
                if let Some(id) = self.terminal_tools.get(&snapshot.terminal_id).cloned() {
                    let failed = snapshot.exit_status.as_ref().is_some_and(|status| {
                        status.exit_code.is_some_and(|code| code != 0) || status.signal.is_some()
                    });
                    self.push_tool(&id, failed);
                }
                true
            }
            _ => false,
        }
    }

    fn push_tool(&mut self, id: &str, failed: bool) {
        let Some(entry) = self.tools.get_mut(id) else {
            return;
        };
        if entry.emitted {
            return;
        }
        entry.emitted = true;
        let title = if entry.title.trim().is_empty() {
            "tool".to_string()
        } else {
            entry.title.trim().to_string()
        };
        let suffix = if failed { " (failed)" } else { "" };
        self.entries.push(format!("{title}{suffix}"));
    }

    fn push(&mut self, text: String) {
        let text = text.trim();
        if !text.is_empty() {
            self.entries.push(text.to_string());
        }
    }

    fn render(&self) -> String {
        render_activity_log(&self.entries)
    }

    /// Number of entries captured so far; used as a watermark so a resumed run
    /// or a cancel reports only what happened since the last delivered report.
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// Renders only the entries appended since `watermark` (a value previously
    /// returned by `len`), eliding the middle the same way `render` does. A
    /// `watermark` of `0` yields exactly what `render` would.
    fn render_since(&self, watermark: usize) -> String {
        let start = watermark.min(self.entries.len());
        if start == 0 {
            return self.render();
        }
        let body = if self.entries[start..].is_empty() {
            "[no subagent activity since the last report]".to_string()
        } else {
            self.entries[start..].join("\n\n")
        };
        elide_middle(&body, SUBAGENT_ACTIVITY_LOG_LIMIT)
    }
}

fn agent_prefix_before_tool_result(text: &str) -> Option<String> {
    let marker = "\n→ ";
    let Some(index) = text.rfind(marker) else {
        return (!text.trim_start().starts_with("**agent**:\n→ "))
            .then(|| text.trim().to_string())
            .filter(|value| !value.is_empty());
    };
    let mut prefix = text[..index].trim_end();
    while let Some((before, last)) = prefix.rsplit_once('\n') {
        if last.trim_start().starts_with("// ") {
            prefix = before.trim_end();
        } else {
            break;
        }
    }
    let prefix = prefix.trim();
    (prefix != "**agent**:" && !prefix.is_empty()).then(|| prefix.to_string())
}

fn render_activity_log(entries: &[String]) -> String {
    let body = if entries.is_empty() {
        "[no subagent activity checkpoints captured]".to_string()
    } else {
        entries.join("\n\n")
    };
    elide_middle(&body, SUBAGENT_ACTIVITY_LOG_LIMIT)
}

fn elide_middle(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(SUBAGENT_ACTIVITY_LOG_HEAD).collect();
    let tail_start = text
        .chars()
        .count()
        .saturating_sub(SUBAGENT_ACTIVITY_LOG_TAIL);
    let tail: String = text.chars().skip(tail_start).collect();
    format!("{head}{SUBAGENT_ACTIVITY_LOG_ELISION}{tail}")
}

fn with_workspace_diff(
    message: &str,
    activity_log: &str,
    delta: Option<&WorkspaceDelta>,
) -> String {
    let activity_log = elide_middle(activity_log, SUBAGENT_ACTIVITY_LOG_LIMIT);
    let activity_block = format!("<activity_summary>\n{activity_log}\n</activity_summary>");
    let Some(delta) = delta else {
        return format!(
            "{message}\n\n{activity_block}\n\n<workspace_diff scope=\"subagent\">\n[workspace delta unavailable because the supervisor failed]\n</workspace_diff>"
        );
    };
    let diff = delta.review_patch().unwrap_or_else(|| delta.receipt());
    let mut result = format!(
        "{message}\n\n{activity_block}\n\n<workspace_diff scope=\"subagent\">\n{diff}\n</workspace_diff>"
    );
    if delta.changed() {
        result.push_str("\n\n");
        result.push_str(SUBAGENT_REVIEW_TEXT);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deepswe::Row;
    use crate::roster::{AcpInventory, AdapterKind, AdapterLaunch};
    use agent_client_protocol::schema::v1::{
        ContentBlock, ContentChunk, TextContent, ToolCallUpdate, ToolCallUpdateFields,
    };

    fn init_repo(root: &Path) {
        for args in [
            ["init", "-q"].as_slice(),
            ["config", "user.email", "mjolnir@example.test"].as_slice(),
            ["config", "user.name", "Mjolnir Tests"].as_slice(),
            ["commit", "--allow-empty", "-qm", "baseline"].as_slice(),
        ] {
            let output = std::process::Command::new("git")
                .current_dir(root)
                .args(args)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    fn role(model: &str, source_id: &str, ranked: bool) -> ResolvedAgent {
        ResolvedAgent {
            model: Row {
                model: model.into(),
                reasoning_effort: None,
                pass_at_1: 0.5,
                mean_cost_usd: 1.0,
            },
            model_value: format!("{model}-value"),
            launch: AdapterLaunch {
                kind: AdapterKind::from_source_id(source_id).unwrap_or(AdapterKind::Custom),
                source_id: source_id.into(),
                command: PathBuf::from(source_id),
                args: Vec::new(),
                env: HashMap::new(),
            },
            ranked,
            reasoning_effort: None,
        }
    }

    fn test_roster() -> Roster {
        let default = role("gpt-y", "codex-acp", true);
        Roster {
            primary: role("gpt-x", "codex-acp", true),
            subagent_default: Some(default.clone()),
            available: vec![
                role("gpt-x", "codex-acp", true),
                default,
                role("claude-a", "claude-acp", true),
                role("claude-b", "claude-acp", false),
            ],
            choices: Vec::new(),
            warnings: Vec::new(),
            inventory: AcpInventory::default(),
        }
    }

    fn test_config() -> Config {
        Config {
            display_label: "subagent".into(),
            command: PathBuf::from("unused"),
            args: Vec::new(),
            env: HashMap::new(),
            agent_stderr: None,
            role_config: Some(acp::RuntimeRoleConfig {
                label: LABEL.to_string(),
                model_id: "gpt-y".to_string(),
                model_value: "gpt-y-value".to_string(),
                adapter_source_id: "codex-acp".to_string(),
                permission: None,
                session_tag: None,
                reasoning_effort: None,
            }),
            subagent_handoff_counter: None,
            active_implementation_workers: ActiveSubagentWorkers::default(),
            max_parallel: 2,
            snapshot_exclusions: Vec::new(),
            id_allocator: SubagentIdAllocator::default(),
            headless_permission_mode: None,
            role_pool: None,
            quota_gate: None,
            inventory: Arc::new(RwLock::new(SubagentInventory::from_roster(&test_roster()))),
            reports: None,
            preamble: SUBAGENT_PREAMBLE.to_string(),
            mcp_servers: Vec::new(),
            usage_seat: Seat::Subagent,
            retain_after_completion: true,
            warm: Arc::default(),
        }
    }

    fn test_context() -> RunContext {
        RunContext {
            cwd: PathBuf::from("/workspace"),
            additional_directories: Vec::new(),
            snapshot_exclusions: Vec::new(),
            fs_max_text_bytes: 1,
            access_mode: RuntimeAccessMode::Full,
        }
    }

    #[test]
    fn fixed_config_stays_on_the_resolved_supervisor_role() {
        let config = Config::for_resolved_agent(role("primary-model", "claude-acp", true), None)
            .with_preamble("review preamble")
            .with_mcp_servers(Vec::new())
            .with_usage_seat(Seat::Review)
            .with_retain_after_completion(true);
        assert!(config.role_pool.is_none());
        assert_eq!(config.current_agent(), "claude-acp");
        assert_eq!(config.current_model(), "primary-model");
        assert_eq!(config.preamble, "review preamble");
        assert_eq!(config.usage_seat, Seat::Review);
        assert!(config.retain_after_completion);
    }

    #[test]
    fn report_injection_escapes_attributes_and_appends_instruction() {
        let report = SubagentReport {
            subagent_id: 7,
            label: "mimir \"core\"".to_string(),
            agent: "codex<acp>".to_string(),
            model: "gpt&review".to_string(),
            outcome: SubagentOutcome::Completed,
            final_message: "one finding".to_string(),
            slim_activity: "read the caller".to_string(),
            workspace_diff: None,
            elapsed: Duration::from_secs(61),
        };
        let rendered = format_report_injection(&[report], "Vet this report.");
        assert!(rendered.contains("label=\"mimir &quot;core&quot;\""));
        assert!(rendered.contains("agent=\"codex&lt;acp&gt;\""));
        assert!(rendered.contains("model=\"gpt&amp;review\""));
        assert!(rendered.contains("elapsed=\"1m01s\""));
        assert!(rendered.contains("[workspace snapshot unavailable"));
        assert!(rendered.ends_with("Vet this report."));
    }

    fn test_mcp_handler(controller: Controller) -> McpHandler {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        McpHandler::new(test_config(), test_context(), ui_tx, controller)
    }

    fn tool_result_text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|content| content.as_text())
            .map(|text| text.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn collector_returns_last_agent_message() {
        let mut collector = AgentMessageCollector::new();
        collector.observe(&SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("first")),
        )));
        collector.observe(&SessionUpdate::ToolCall(
            agent_client_protocol::schema::v1::ToolCall::new("tool", "work"),
        ));
        collector.observe(&SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("final")),
        )));
        assert_eq!(collector.finish().expect("message"), "final");
        assert!(AgentMessageCollector::new().finish().is_err());
    }

    #[test]
    fn tool_arguments_are_strict() {
        let minimal: CreateSubagentArgs =
            serde_json::from_str(r#"{"prompt":"fix it"}"#).expect("valid arguments");
        assert_eq!(minimal.prompt, "fix it");
        assert_eq!(minimal.agent, None);
        assert_eq!(minimal.resume, None);

        let full: CreateSubagentArgs = serde_json::from_str(
            r#"{"prompt":"fix it","agent":"codex-acp","model":"gpt-y","label":"fix","cwd":"/tmp/worktree","resume":3}"#,
        )
        .expect("valid arguments");
        assert_eq!(full.agent.as_deref(), Some("codex-acp"));
        assert_eq!(full.cwd, Some(PathBuf::from("/tmp/worktree")));
        assert_eq!(full.resume, Some(3));

        assert!(
            serde_json::from_str::<CreateSubagentArgs>(r#"{"prompt":"fix it","extra":true}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<CreateSubagentArgs>("{}").is_err());

        let cancel: SubagentCancelArgs =
            serde_json::from_str(r#"{"subagent_id":7}"#).expect("valid cancel args");
        assert_eq!(cancel.subagent_id, 7);
        assert!(
            serde_json::from_str::<SubagentCancelArgs>(r#"{"subagent_id":7,"extra":true}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<SubagentCancelArgs>("{}").is_err());
    }

    #[test]
    fn only_the_two_subagent_tools_are_registered() {
        let router = McpHandler::tool_router();
        assert!(router.get("create_subagent").is_some());
        assert!(router.get("subagent_cancel").is_some());
        assert!(router.get("code_agent").is_none());
        assert!(router.get("explore_agent").is_none());
    }

    #[test]
    fn server_info_and_tool_description_carry_the_inventory_and_policy() {
        let handler = test_mcp_handler(Controller::default());
        let info = handler.server_info();
        let instructions = info.instructions.as_deref().expect("server instructions");
        assert!(instructions.contains(SERVER_DELEGATION_GUIDANCE));
        assert!(instructions.contains(PRIMARY_SESSION_DIRECTIVE));
        assert!(instructions.contains("- codex-acp (Codex): gpt-x, gpt-y*"));

        let tools = handler.described_tools();
        let create = tools
            .iter()
            .find(|tool| tool.name == "create_subagent")
            .expect("create_subagent is advertised");
        let description = create.description.as_deref().expect("description");
        assert!(description.contains("RETURNS IMMEDIATELY"));
        assert!(description.contains("Available agents and models:"));
        assert!(description.contains("- claude-acp (Claude Code): claude-a, claude-b"));
        assert!(description.contains("(* = default when agent and model are omitted)"));

        let cancel = tools
            .iter()
            .find(|tool| tool.name == "subagent_cancel")
            .expect("subagent_cancel is advertised");
        assert!(
            !cancel
                .description
                .as_deref()
                .expect("description")
                .contains("Available agents"),
            "only the spawn tool carries the inventory"
        );
    }

    #[test]
    fn resolve_session_matrix() {
        let config = test_config();

        let default = config.resolve_session(None, None).expect("default session");
        assert_eq!(default.agent, "codex-acp");
        assert_eq!(default.model, "gpt-y");
        assert!(
            default.role.is_none(),
            "the default path keeps the RolePool (and its quota failover)"
        );

        let by_model = config
            .resolve_session(None, Some("CLAUDE-A"))
            .expect("model-only routing is case-insensitive");
        assert_eq!(by_model.agent, "claude-acp");
        assert_eq!(by_model.model, "claude-a");
        assert!(
            by_model.role.is_some(),
            "an explicit pick bypasses the pool"
        );

        let by_value = config
            .resolve_session(None, Some("claude-b-value"))
            .expect("the raw advertised value also resolves");
        assert_eq!(by_value.model, "claude-b");

        let by_agent = config
            .resolve_session(Some("claude-acp"), None)
            .expect("agent-only picks that server's best ranked model");
        assert_eq!(by_agent.model, "claude-a");

        let pair = config
            .resolve_session(Some("codex-acp"), Some("gpt-x"))
            .expect("a valid pair");
        assert_eq!(
            (pair.agent.as_str(), pair.model.as_str()),
            ("codex-acp", "gpt-x")
        );

        let unknown_agent = config
            .resolve_session(Some("nope-acp"), None)
            .expect_err("unknown agent");
        assert!(unknown_agent.message.contains("unknown agent nope-acp"));
        assert!(
            unknown_agent
                .message
                .contains("valid agents: [codex-acp, claude-acp]")
        );

        let unknown_model = config
            .resolve_session(None, Some("gpt-z"))
            .expect_err("unknown model");
        assert!(
            unknown_model
                .message
                .contains("no agent advertises model gpt-z")
        );
        assert!(unknown_model.message.contains("valid models:"));

        let mismatch = config
            .resolve_session(Some("codex-acp"), Some("claude-a"))
            .expect_err("pair mismatch");
        assert!(
            mismatch
                .message
                .contains("agent codex-acp does not advertise model claude-a")
        );
    }

    #[test]
    fn an_empty_inventory_rejects_explicit_picks_and_renders_nothing() {
        let inventory = SubagentInventory::default();
        assert!(inventory.render("codex-acp", "gpt-y").is_empty());
        let error = inventory
            .resolve(Some("codex-acp"), None)
            .expect_err("nothing is launchable");
        assert!(error.message.contains("omit agent and model"));
    }

    #[tokio::test]
    async fn quota_blocked_explicit_picks_fail_fast() {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let mut config = test_config();
        config.quota_gate = Some(crate::quota::Gate::new(PathBuf::from("."), ui_tx));
        // Kimi/Anvil/Custom report `Unavailable`, never `NearLimit`, so a gate
        // that cannot answer must not block the launch.
        let role = role("claude-a", "claude-acp", true);
        assert!(config.check_explicit_quota(&role).await.is_ok());
    }

    #[tokio::test]
    async fn pool_admits_to_capacity_then_rejects_naming_active_ids() {
        let controller = Controller::default();
        controller
            .configure(
                2,
                ActiveSubagentWorkers::default(),
                SubagentIdAllocator::default(),
            )
            .await;
        let root = PathBuf::from("/workspace");

        let first = controller.begin(root.clone()).await.expect("first");
        let second = controller.begin(root.clone()).await.expect("second");
        assert_eq!(controller.active_count().await, 2);

        let full = controller
            .begin(root.clone())
            .await
            .expect_err("the pool is at capacity");
        assert_eq!(full.capacity, 2);
        assert_eq!(full.active, vec![first.subagent_id, second.subagent_id]);
        let message = full.message();
        assert!(message.contains("#1, #2"));
        assert!(message.contains("2 of 2 slots"));
        assert!(message.contains("Nothing was queued"));

        controller.finish(first.subagent_id).await;
        assert!(
            controller.begin(root).await.is_ok(),
            "a freed slot re-admits"
        );
    }

    #[tokio::test]
    async fn overlapping_runs_in_one_workspace_are_counted_for_diff_suppression() {
        let controller = Controller::default();
        controller
            .configure(
                4,
                ActiveSubagentWorkers::default(),
                SubagentIdAllocator::default(),
            )
            .await;
        let shared = PathBuf::from("/workspace");
        let other = PathBuf::from("/elsewhere");

        let first = controller.begin(shared.clone()).await.expect("first");
        let elsewhere = controller.begin(other).await.expect("elsewhere");
        assert_eq!(first.overlap.load(Ordering::Acquire), 0);

        let second = controller.begin(shared).await.expect("second");
        assert_eq!(second.overlap.load(Ordering::Acquire), 1);
        assert_eq!(
            first.overlap.load(Ordering::Acquire),
            1,
            "the earlier run learns it no longer owns the workspace alone"
        );
        assert_eq!(
            elsewhere.overlap.load(Ordering::Acquire),
            0,
            "a different workspace root does not overlap"
        );

        assert_eq!(
            report_workspace_diff(None, 2).as_deref(),
            Some(
                "omitted: 2 subagents shared this workspace during the run — inspect git diff yourself"
            )
        );
        assert!(
            report_workspace_diff(None, 1)
                .expect("note")
                .contains("1 subagent shared")
        );
        let delta = WorkspaceDelta::changed_for_test("diff --git a/x b/x\n+done\n".to_string());
        assert_eq!(
            report_workspace_diff(Some(&delta), 0).as_deref(),
            Some("diff --git a/x b/x\n+done\n")
        );
        assert!(report_workspace_diff(None, 0).is_none());
    }

    #[tokio::test]
    async fn retained_runs_free_their_slot_and_stop_counting_as_active_workers() {
        let controller = Controller::default();
        let workers = ActiveSubagentWorkers::default();
        let counted = workers.subscribe();
        controller
            .configure(1, workers, SubagentIdAllocator::default())
            .await;
        let root = PathBuf::from("/workspace");

        let admission = controller.begin(root.clone()).await.expect("admitted");
        assert_eq!(*counted.borrow(), 1);
        let (commands, _commands_rx) = mpsc::unbounded_channel::<UiCommand>();
        controller.attach(admission.subagent_id, commands).await;

        controller.retain_complete(admission.subagent_id).await;
        assert_eq!(
            *counted.borrow(),
            0,
            "a retained run is idle and must not hold the review gate open"
        );
        let replacement = controller
            .begin(root)
            .await
            .expect("a retained run frees its pool slot");
        controller.finish(replacement.subagent_id).await;

        assert!(
            controller
                .resume_retained(admission.subagent_id)
                .await
                .is_ok()
        );
        assert_eq!(*counted.borrow(), 1, "a resumed run is active again");
        controller.finish(admission.subagent_id).await;
        assert_eq!(*counted.borrow(), 0);
    }

    #[tokio::test]
    async fn resume_is_rejected_when_the_pool_is_full() {
        let controller = Controller::default();
        controller
            .configure(
                1,
                ActiveSubagentWorkers::default(),
                SubagentIdAllocator::default(),
            )
            .await;
        let root = PathBuf::from("/workspace");
        let retained = controller.begin(root.clone()).await.expect("retained");
        let (commands, _commands_rx) = mpsc::unbounded_channel::<UiCommand>();
        controller.attach(retained.subagent_id, commands).await;
        controller.retain_complete(retained.subagent_id).await;
        let running = controller.begin(root).await.expect("running");

        let full = controller
            .resume_retained(retained.subagent_id)
            .await
            .expect_err("no free slot");
        assert_eq!(full.active, vec![running.subagent_id]);
    }

    #[test]
    fn retention_reaps_the_oldest_session_past_the_limit() {
        let registry = SubagentRegistry::default();
        let mut receivers = Vec::new();
        for id in 1..=2 {
            let (tx, rx) = mpsc::unbounded_channel::<WorkerRequest>();
            receivers.push((id, rx));
            assert!(registry.insert_retained(id, tx, 2).is_empty());
        }
        assert_eq!(registry.retained_ids(), vec![1, 2]);

        let (tx, _rx) = mpsc::unbounded_channel::<WorkerRequest>();
        let reaped = registry.insert_retained(3, tx, 2);
        assert_eq!(reaped.len(), 1, "the oldest retained session is reaped");
        let _ = reaped[0].send(WorkerRequest::Supersede);
        assert!(matches!(
            receivers[0].1.try_recv(),
            Ok(WorkerRequest::Supersede)
        ));
        assert_eq!(registry.retained_ids(), vec![2, 3]);
        assert!(
            registry.take(1).is_none(),
            "a reaped run leaves the registry"
        );
    }

    #[tokio::test]
    async fn resume_rejects_unknown_and_still_running_subagents() {
        let controller = Controller::default();
        controller
            .configure(
                2,
                ActiveSubagentWorkers::default(),
                SubagentIdAllocator::default(),
            )
            .await;
        let handler = test_mcp_handler(controller);
        let spec = handler
            .config
            .resolve_session(None, None)
            .expect("default session");

        let unknown = handler
            .resume_subagent(99, "keep going".to_string(), "label", &spec)
            .await
            .expect_err("unknown id is a protocol error");
        assert!(
            unknown
                .message
                .contains("subagent_id 99 is not a known subagent")
        );

        let (control_tx, _control_rx) = mpsc::unbounded_channel::<WorkerRequest>();
        handler.runs.insert_running(4, control_tx);
        let running = handler
            .resume_subagent(4, "keep going".to_string(), "label", &spec)
            .await
            .expect("still-running resume is a tool-level error");
        assert_eq!(running.is_error, Some(true));
        assert!(tool_result_text(&running).contains("subagent #4 is still running"));
        assert!(
            handler.runs.take(4).is_some(),
            "a rejected resume leaves the run registered"
        );
    }

    #[tokio::test]
    async fn create_subagent_rejects_an_empty_prompt_and_a_full_pool() {
        let controller = Controller::default();
        controller
            .configure(
                1,
                ActiveSubagentWorkers::default(),
                SubagentIdAllocator::default(),
            )
            .await;
        let handler = test_mcp_handler(controller.clone());

        assert!(
            handler
                .create_subagent(Parameters(CreateSubagentArgs {
                    prompt: "  ".to_string(),
                    agent: None,
                    model: None,
                    label: None,
                    cwd: None,
                    resume: None,
                }))
                .await
                .is_err()
        );

        let occupied = controller
            .begin(canonical_root(&handler.context.cwd).await)
            .await
            .expect("occupy the only slot");
        let rejected = handler
            .create_subagent(Parameters(CreateSubagentArgs {
                prompt: "do the thing".to_string(),
                agent: None,
                model: None,
                label: None,
                cwd: None,
                resume: None,
            }))
            .await
            .expect("pool-full is a tool-level error");
        assert_eq!(rejected.is_error, Some(true));
        let text = tool_result_text(&rejected);
        assert!(text.contains(&format!("#{}", occupied.subagent_id)));
        assert!(text.contains("Nothing was queued"));
    }

    /// The discrete-review gate reads this counter, so it has to see every
    /// delegation the turn actually made -- and none it did not.
    #[tokio::test]
    async fn every_admitted_spawn_counts_as_a_handoff_including_a_resume() {
        let counter = Arc::new(AtomicUsize::new(0));
        let controller = Controller::default();
        controller
            .configure(
                1,
                ActiveSubagentWorkers::default(),
                SubagentIdAllocator::default(),
            )
            .await;
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let mut config = test_config();
        config.subagent_handoff_counter = Some(counter.clone());
        let handler = McpHandler::new(config, test_context(), ui_tx, controller.clone());
        let spec = handler
            .config
            .resolve_session(None, None)
            .expect("default session");

        // Rejected by a full pool: nothing started, so nothing is counted.
        let occupied = controller
            .begin(canonical_root(&handler.context.cwd).await)
            .await
            .expect("occupy the only slot");
        let rejected = handler
            .create_subagent(Parameters(CreateSubagentArgs {
                prompt: "do the thing".to_string(),
                agent: None,
                model: None,
                label: None,
                cwd: None,
                resume: None,
            }))
            .await
            .expect("pool-full is a tool-level error");
        assert_eq!(rejected.is_error, Some(true));
        assert_eq!(counter.load(Ordering::Acquire), 0);
        controller.finish(occupied.subagent_id).await;

        // A resume re-admits a retained session: still a delegation.
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<WorkerRequest>();
        let _ = handler.runs.insert_retained(7, control_tx, 2);
        let resumed = handler
            .resume_subagent(7, "keep going".to_string(), "follow-up", &spec)
            .await
            .expect("resume is admitted");
        assert_eq!(resumed.is_error, Some(false));
        assert!(matches!(
            control_rx.try_recv(),
            Ok(WorkerRequest::Continue { .. })
        ));
        assert_eq!(counter.load(Ordering::Acquire), 1);
    }

    #[test]
    fn started_result_is_structured_and_tells_the_caller_not_to_poll() {
        let result = started_tool_result(3, "fix-tests", "codex-acp", "gpt-y");
        assert_eq!(result.is_error, Some(false));
        let text = tool_result_text(&result);
        assert!(text.contains("subagent #3 (fix-tests) started on codex-acp/gpt-y"));
        assert!(text.contains("Do not poll"));
        assert!(text.contains("<subagent_result id=\"3\">"));
        let structured = result.structured_content.expect("structured content");
        assert_eq!(structured["subagentId"], 3);
        assert_eq!(structured["status"], "started");
        assert_eq!(structured["agent"], "codex-acp");
        assert_eq!(structured["model"], "gpt-y");
        assert_eq!(structured["label"], "fix-tests");
    }

    #[test]
    fn default_label_is_a_bounded_first_line_excerpt() {
        assert_eq!(
            default_label("  fix the parser\nmore detail"),
            "fix the parser"
        );
        let long = "a".repeat(DEFAULT_LABEL_CHARS + 10);
        let label = default_label(&long);
        assert!(label.ends_with('…'));
        assert_eq!(label.chars().count(), DEFAULT_LABEL_CHARS + 1);
        assert_eq!(default_label("   "), "subagent");
    }

    #[tokio::test]
    async fn cancel_and_shutdown_record_their_termination_cause() {
        let controller = Controller::default();
        let root = PathBuf::from("/workspace");
        let cancelled = controller.begin(root.clone()).await.expect("cancelled run");
        assert!(controller.cancel().await);
        assert_eq!(
            cancelled.termination.cause(),
            TerminationCause::UserCancelled
        );
        controller.finish(cancelled.subagent_id).await;

        let shutdown = controller.begin(root).await.expect("shutdown run");
        assert!(controller.shutdown().await);
        assert_eq!(
            controller
                .termination(shutdown.subagent_id)
                .await
                .expect("termination")
                .cause(),
            TerminationCause::RuntimeShutdown
        );
        controller.finish(shutdown.subagent_id).await;
    }

    #[test]
    fn idle_retained_runtime_shutdown_is_clean_for_outcome_and_telemetry() {
        let shutdown = retained_termination_result(TerminationCause::RuntimeShutdown);
        assert!(
            shutdown.is_ok(),
            "reaping an idle retained session must not look like an agent failure"
        );
        assert_eq!(outcome_for(&shutdown), SubagentOutcome::Completed);

        let cancelled = retained_termination_result(TerminationCause::UserCancelled);
        assert!(cancelled.is_err());
        assert_eq!(outcome_for(&cancelled), SubagentOutcome::Cancelled);
    }

    #[tokio::test]
    async fn shutdown_requested_while_starting_reaches_the_nested_runtime() {
        let controller = Controller::default();
        let admission = controller
            .begin(PathBuf::from("/workspace"))
            .await
            .expect("admitted");
        assert!(controller.shutdown().await);
        let (commands, mut receiver) = mpsc::unbounded_channel();
        controller.attach(admission.subagent_id, commands).await;
        assert!(matches!(receiver.recv().await, Some(UiCommand::Shutdown)));
    }

    #[tokio::test]
    async fn outer_runtime_shutdown_waits_for_the_worker_slot_release() {
        let controller = Controller::default();
        let admission = controller
            .begin(PathBuf::from("/workspace"))
            .await
            .expect("admitted");
        let shutdown_controller = controller.clone();
        let mut shutdown =
            tokio::spawn(async move { shutdown_controller.shutdown_and_wait().await });

        admission.termination.cancelled().await;
        assert_eq!(
            admission.termination.cause(),
            TerminationCause::RuntimeShutdown
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut shutdown)
                .await
                .is_err(),
            "the outer runtime returned before the worker supervisor"
        );

        controller.finish(admission.subagent_id).await;
        controller.wait_until_absent(admission.subagent_id).await;
        assert!(shutdown.await.expect("shutdown task"));
    }

    #[tokio::test]
    async fn user_cancellation_waits_for_the_worker_slot_release() {
        let controller = Controller::default();
        let admission = controller
            .begin(PathBuf::from("/workspace"))
            .await
            .expect("admitted");
        let cancel_controller = controller.clone();
        let mut cancel = tokio::spawn(async move { cancel_controller.cancel_and_wait().await });

        admission.termination.cancelled().await;
        assert_eq!(
            admission.termination.cause(),
            TerminationCause::UserCancelled
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut cancel)
                .await
                .is_err(),
            "the cancellation returned before the worker supervisor"
        );

        controller.finish(admission.subagent_id).await;
        controller.wait_until_absent(admission.subagent_id).await;
        assert!(cancel.await.expect("cancel task"));
    }

    #[tokio::test]
    async fn dropping_a_pending_admission_cannot_orphan_a_controller_slot() {
        let controller = Controller::default();
        let state_lock = controller.state.lock().await;
        let pending = tokio::spawn({
            let controller = controller.clone();
            async move { controller.begin(PathBuf::from("/workspace")).await.is_ok() }
        });
        tokio::task::yield_now().await;
        pending.abort();
        assert!(pending.await.is_err());
        drop(state_lock);

        assert!(controller.begin(PathBuf::from("/workspace")).await.is_ok());
    }

    #[test]
    fn activity_transcript_uses_boundary_checkpoints_without_tool_outputs() {
        let mut tracker = BoundaryTracker::default();
        let mut transcript = SubagentTranscript::default();
        let events = [
            UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::Text(TextContent::new("I will validate.")),
            ))),
            UiEvent::SessionUpdate(SessionUpdate::ToolCall(
                agent_client_protocol::schema::v1::ToolCall::new("tool", "Run `cargo test`")
                    .status(ToolCallStatus::Failed),
            )),
        ];
        for event in events {
            let boundary = tracker.observe(&event);
            transcript.observe(&event, boundary.as_ref());
        }
        let rendered = transcript.render();
        assert!(rendered.contains("I will validate."));
        assert!(rendered.contains("Run `cargo test` (failed)"));
        assert!(!rendered.contains("⇒ error"));
    }

    #[test]
    fn activity_transcript_render_since_excludes_entries_before_the_watermark() {
        let mut transcript = SubagentTranscript::default();
        transcript.push("before the report".to_string());
        let watermark = transcript.len();
        transcript.push("after the report, first".to_string());
        transcript.push("after the report, second".to_string());

        let tail = transcript.render_since(watermark);
        assert!(!tail.contains("before the report"));
        assert!(tail.contains("after the report, first"));
        assert!(tail.contains("after the report, second"));

        assert_eq!(transcript.render_since(0), transcript.render());
        assert_eq!(
            transcript.render_since(transcript.len()),
            "[no subagent activity since the last report]"
        );
    }

    #[test]
    fn activity_log_elides_the_middle_at_the_cap() {
        let entries = vec![format!(
            "{}MIDDLE{}",
            "a".repeat(SUBAGENT_ACTIVITY_LOG_HEAD + 500),
            "z".repeat(SUBAGENT_ACTIVITY_LOG_TAIL + 500)
        )];
        let rendered = render_activity_log(&entries);
        assert!(rendered.contains(SUBAGENT_ACTIVITY_LOG_ELISION.trim()));
        assert!(rendered.starts_with(&"a".repeat(100)));
        assert!(rendered.ends_with(&"z".repeat(100)));
        assert!(!rendered.contains("MIDDLE"));
    }

    #[test]
    fn cancelled_tool_result_states_that_edits_are_kept() {
        let delta = WorkspaceDelta::changed_for_test("diff --git a/x b/x\n+partial\n".to_string());
        let released = SubagentRunResult {
            outcome: Ok("released".to_string()),
            workspace_delta: Some(delta.clone()),
            activity_log: "cancel activity".to_string(),
            cancelled_while_running: false,
        };
        let text = tool_result_text(&cancelled_tool_result(&released));
        assert!(text.contains("retained session was released"));
        assert!(text.contains("did not revert"));
        assert!(text.contains("+partial"));

        let interrupted = SubagentRunResult {
            outcome: Err(anyhow!("the subagent was cancelled while still working")),
            workspace_delta: Some(delta),
            activity_log: "activity since it started".to_string(),
            cancelled_while_running: true,
        };
        let rendered = cancelled_tool_result(&interrupted);
        assert_eq!(rendered.is_error, Some(false));
        let text = tool_result_text(&rendered);
        assert!(text.contains("cancelled while still working"));
        assert!(text.contains("No report will be injected"));
        assert!(text.contains("activity since it started"));
    }

    #[test]
    fn continuation_prompt_preserves_the_callers_guidance() {
        let prompt = continuation_prompt("focus the parser tests");
        assert!(prompt.contains("previous progress is preserved"));
        assert!(prompt.ends_with("focus the parser tests"));
    }

    #[test]
    fn report_bus_accounting_opens_at_admission_and_closes_on_handling() {
        let (bus, mut rx) = SubagentReportBus::channel();
        assert_eq!(bus.pending(), 0);
        bus.open();
        bus.open();
        assert_eq!(bus.pending(), 2);
        bus.deliver(SubagentReport {
            subagent_id: 1,
            label: "one".to_string(),
            agent: "codex-acp".to_string(),
            model: "gpt-y".to_string(),
            outcome: SubagentOutcome::Completed,
            final_message: "done".to_string(),
            slim_activity: String::new(),
            workspace_diff: None,
            elapsed: Duration::ZERO,
        });
        assert_eq!(
            bus.pending(),
            2,
            "delivery alone does not close the account"
        );
        assert!(rx.try_recv().is_ok());
        bus.close();
        bus.close();
        bus.close();
        assert_eq!(bus.pending(), 0, "closing saturates rather than wrapping");
    }

    #[test]
    fn subagent_preamble_frames_the_brief_as_evidence() {
        assert!(SUBAGENT_PREAMBLE.contains("not ground truth"));
        assert!(SUBAGENT_PREAMBLE.contains("project's own checks"));
        assert!(SUBAGENT_PREAMBLE.contains("Your final message is the report"));
        assert!(!SUBAGENT_PREAMBLE.contains("Eitri"));
        assert!(!PRIMARY_SESSION_DIRECTIVE.contains("Thor"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("Never poll"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("end your turn"));
    }

    #[tokio::test]
    async fn explicit_cwd_becomes_the_only_nested_workspace_root() {
        let primary = tempfile::tempdir().expect("primary workspace");
        let delegated = tempfile::tempdir().expect("delegated worktree");
        let context = RunContext {
            cwd: std::fs::canonicalize(primary.path()).expect("canonical primary"),
            additional_directories: vec![
                std::fs::canonicalize(delegated.path()).expect("canonical delegated worktree"),
            ],
            snapshot_exclusions: Vec::new(),
            fs_max_text_bytes: 1,
            access_mode: RuntimeAccessMode::Full,
        };

        let resolved = resolve_subagent_context(&context, Some(delegated.path()))
            .await
            .expect("authorized delegated worktree");
        assert_eq!(
            resolved.cwd,
            std::fs::canonicalize(delegated.path()).expect("canonical delegated worktree")
        );
        assert!(resolved.additional_directories.is_empty());
    }

    #[tokio::test]
    async fn explicit_cwd_rejects_an_unauthorized_sibling() {
        let workspace = tempfile::tempdir().expect("workspace parent");
        let primary = workspace.path().join("primary");
        let sibling = workspace.path().join("sibling");
        tokio::fs::create_dir_all(&primary).await.expect("primary");
        tokio::fs::create_dir_all(&sibling).await.expect("sibling");
        let context = RunContext {
            cwd: std::fs::canonicalize(&primary).expect("canonical primary"),
            additional_directories: Vec::new(),
            snapshot_exclusions: Vec::new(),
            fs_max_text_bytes: 1,
            access_mode: RuntimeAccessMode::Full,
        };

        let error = resolve_subagent_context(&context, Some(&sibling))
            .await
            .expect_err("a sibling is not an authorized workspace root");
        assert!(error.message.contains("authorized workspace roots"));
        assert!(error.message.contains("additional workspace root"));
    }

    #[tokio::test]
    async fn a_run_snapshot_reports_only_its_own_workspace_roots() {
        let workspace = tempfile::tempdir().expect("workspace parent");
        let primary = workspace.path().join("primary");
        let external = workspace.path().join("external");
        std::fs::create_dir_all(&primary).expect("primary directory");
        std::fs::create_dir_all(&external).expect("external directory");
        init_repo(&primary);
        init_repo(&external);
        let primary = std::fs::canonicalize(&primary).expect("canonical primary");
        let external = std::fs::canonicalize(&external).expect("canonical external");
        let runtime_log = external.join("mj-debug.log");
        let outer = RunContext {
            cwd: primary.clone(),
            additional_directories: vec![external.clone()],
            snapshot_exclusions: vec![runtime_log.clone()],
            fs_max_text_bytes: 1,
            access_mode: RuntimeAccessMode::Full,
        };

        let delegated = resolve_subagent_context(&outer, Some(&external))
            .await
            .expect("authorized external worktree");
        assert_eq!(subagent_workspace_roots(&delegated), vec![external.clone()]);
        let snapshot = capture_workspace_snapshot(&delegated).await;

        std::fs::write(external.join("subagent-change.txt"), "changed\n").expect("change");
        std::fs::write(runtime_log, "Mjolnir runtime output\n").expect("runtime log");

        let delta = snapshot.delta().await;
        assert!(delta.changed());
        assert!(
            delta
                .receipt()
                .contains(&format!("Repository: {}", external.display()))
        );
        assert!(
            !delta
                .receipt()
                .contains(&format!("Repository: {}", primary.display()))
        );
        assert!(delta.receipt().contains("subagent-change.txt"));
        assert!(!delta.receipt().contains("mj-debug.log"));
    }

    #[tokio::test]
    async fn warm_pool_claims_only_an_exact_context_and_role_match() {
        let config = test_config();
        let context = test_context();
        let (commands, _command_rx) = mpsc::unbounded_channel();
        let (_event_tx, events) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(std::future::pending());
        *config.warm.slot.lock().unwrap() = Some(WarmRuntime {
            context: context.clone(),
            role_key: config.role_key(),
            events,
            commands,
            task,
            cancel: cancel.clone(),
        });

        let mut mismatch = context.clone();
        mismatch.cwd = PathBuf::from("/other");
        assert!(config.take_warm(&mismatch).is_none());
        let runtime = config.take_warm(&context).expect("matching warm runtime");
        runtime.cancel.cancel();
        runtime.task.abort();
    }

    #[tokio::test]
    async fn warm_pool_discards_a_runtime_that_failed_during_startup() {
        let config = test_config();
        let context = test_context();
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let (_event_tx, events) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(async { Ok(()) });
        tokio::task::yield_now().await;
        assert!(task.is_finished());
        *config.warm.slot.lock().unwrap() = Some(WarmRuntime {
            context: context.clone(),
            role_key: config.role_key(),
            events,
            commands,
            task,
            cancel: cancel.clone(),
        });

        assert!(config.take_warm(&context).is_none());
        assert!(cancel.is_cancelled());
        assert!(matches!(command_rx.try_recv(), Ok(UiCommand::Shutdown)));
        assert!(config.warm.slot.lock().unwrap().is_none());
    }

    /// Drives a real `run()` end to end against a fake nested ACP runtime
    /// injected through the warm pool, so the report path (not just the
    /// protocol types) is exercised without a real subprocess.
    struct FakeRun {
        controller: Controller,
        subagent_id: u64,
        registry: SubagentRegistry,
        reports: mpsc::UnboundedReceiver<SubagentReport>,
        bus: SubagentReportBus,
        nested_events: mpsc::UnboundedSender<UiEvent>,
        nested_commands: mpsc::UnboundedReceiver<UiCommand>,
        ui_events: mpsc::UnboundedReceiver<UiEvent>,
        _workspace: tempfile::TempDir,
    }

    async fn spawn_fake_run() -> FakeRun {
        spawn_fake_run_with(Vec::new(), true).await
    }

    async fn spawn_fake_run_with(
        images: Vec<PromptImage>,
        retain_after_completion: bool,
    ) -> FakeRun {
        spawn_fake_run_with_visibility(images, retain_after_completion, false).await
    }

    async fn spawn_fake_run_with_visibility(
        images: Vec<PromptImage>,
        retain_after_completion: bool,
        defer_finished_while_retained: bool,
    ) -> FakeRun {
        let workspace = tempfile::tempdir().expect("workspace");
        init_repo(workspace.path());
        let cwd = std::fs::canonicalize(workspace.path()).expect("canonical cwd");
        let context = RunContext {
            cwd: cwd.clone(),
            additional_directories: Vec::new(),
            snapshot_exclusions: Vec::new(),
            fs_max_text_bytes: 1_000_000,
            access_mode: RuntimeAccessMode::Full,
        };
        let controller = Controller::default();
        controller
            .configure(
                2,
                ActiveSubagentWorkers::default(),
                SubagentIdAllocator::default(),
            )
            .await;
        let admission = controller.begin(cwd).await.expect("admitted");

        let (bus, reports) = SubagentReportBus::channel();
        let mut config = test_config();
        config.reports = Some(bus.clone());
        config.retain_after_completion = retain_after_completion;

        let (commands, nested_commands) = mpsc::unbounded_channel();
        let (nested_events, events) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        // The fake nested "process" ends as soon as `run()` cancels it during
        // teardown, exactly like a real ACP runtime task.
        let cancel_signal = cancel.clone();
        let task: JoinHandle<Result<()>> = tokio::spawn(async move {
            cancel_signal.cancelled().await;
            Ok(())
        });
        *config.warm.slot.lock().unwrap() = Some(WarmRuntime {
            context: context.clone(),
            role_key: config.role_key(),
            events,
            commands,
            task,
            cancel,
        });

        let registry = SubagentRegistry::default();
        let (ui_tx, ui_events) = mpsc::unbounded_channel();
        let subagent_id = admission.subagent_id;
        bus.open();
        let mut policy = RunPolicy::configured(&config);
        policy.defer_finished_while_retained = defer_finished_while_retained;
        launch_subagent_worker(
            controller.clone(),
            registry.clone(),
            config,
            context,
            "do the thing".to_string(),
            images,
            "fix-tests".to_string(),
            SessionSpec {
                agent: "codex-acp".to_string(),
                model: "gpt-y".to_string(),
                role: None,
            },
            policy,
            ui_tx,
            admission,
        );

        FakeRun {
            controller,
            subagent_id,
            registry,
            reports,
            bus,
            nested_events,
            nested_commands,
            ui_events,
            _workspace: workspace,
        }
    }

    async fn next_visible_subagent_event(
        events: &mut mpsc::UnboundedReceiver<UiEvent>,
    ) -> SubagentEvent {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let UiEvent::Subagent(event) = events.recv().await.expect("UI event stream") {
                    return event;
                }
            }
        })
        .await
        .expect("visible subagent event")
    }

    #[tokio::test]
    async fn nested_runtime_preserves_warning_severity_for_the_primary_ui() {
        let mut run = spawn_fake_run().await;
        run.nested_events
            .send(UiEvent::SessionStarted {
                session_id: "s1".to_string(),
                resumed: false,
            })
            .expect("session started");
        let _ = run.nested_commands.recv().await.expect("prompt");
        run.nested_events
            .send(UiEvent::Warning("provider rate limit is near".to_string()))
            .expect("warning");

        loop {
            if let SubagentEvent::Status {
                subagent_id,
                kind,
                message,
            } = next_visible_subagent_event(&mut run.ui_events).await
            {
                assert_eq!(subagent_id, run.subagent_id);
                assert_eq!(kind, SubagentStatusKind::Warning);
                assert_eq!(message, "provider rate limit is near");
                break;
            }
        }

        assert!(run.controller.cancel_and_wait().await);
    }

    #[tokio::test]
    async fn a_finished_run_reports_and_retains_its_session_for_resume() {
        let mut run = spawn_fake_run().await;
        run.nested_events
            .send(UiEvent::SessionStarted {
                session_id: "s1".to_string(),
                resumed: false,
            })
            .expect("session started");
        let UiCommand::SendPrompt { text, .. } = run.nested_commands.recv().await.expect("prompt")
        else {
            panic!("expected the standalone brief");
        };
        assert!(text.starts_with(SUBAGENT_PREAMBLE));
        assert!(text.ends_with("do the thing"));

        run.nested_events
            .send(UiEvent::SessionUpdate(SessionUpdate::ToolCall(
                agent_client_protocol::schema::v1::ToolCall::new("t1", "Explore the code")
                    .status(ToolCallStatus::Completed),
            )))
            .expect("tool call");
        run.nested_events
            .send(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new("all green"))),
            )))
            .expect("final message");
        run.nested_events
            .send(UiEvent::PromptDone {
                stop_reason: StopReason::EndTurn,
                usage: None,
            })
            .expect("turn done");

        let report = tokio::time::timeout(Duration::from_secs(5), run.reports.recv())
            .await
            .expect("a report is pushed without any polling")
            .expect("report");
        assert_eq!(report.subagent_id, run.subagent_id);
        assert_eq!(report.label, "fix-tests");
        assert_eq!(report.outcome, SubagentOutcome::Completed);
        assert_eq!(report.final_message, "all green");
        assert!(report.slim_activity.contains("Explore the code"));
        assert!(report.workspace_diff.is_some());
        assert_eq!(
            run.bus.pending(),
            1,
            "the orchestrator has not closed it yet"
        );

        assert_eq!(
            run.registry.retained_ids(),
            vec![run.subagent_id],
            "receiving the report must mean resume is already safe"
        );
        assert_eq!(
            run.controller.active_count().await,
            0,
            "a retained run frees its pool slot"
        );
        loop {
            if let SubagentEvent::Finished {
                subagent_id,
                outcome,
            } = next_visible_subagent_event(&mut run.ui_events).await
            {
                assert_eq!(subagent_id, run.subagent_id);
                assert_eq!(outcome, SubagentOutcome::Completed);
                break;
            }
        }

        let released = run.registry.take(run.subagent_id).expect("retained run");
        let (respond, respond_rx) = oneshot::channel();
        released
            .control
            .send(WorkerRequest::Cancel { respond })
            .expect("release the retained session");
        let result = tokio::time::timeout(Duration::from_secs(5), respond_rx)
            .await
            .expect("release settles")
            .expect("cancel result");
        assert!(!result.cancelled_while_running);
        assert!(result.outcome.is_ok());
    }

    #[tokio::test]
    async fn retained_programmatic_coordinator_stays_visible_until_cancelled() {
        let mut run = spawn_fake_run_with_visibility(Vec::new(), true, true).await;
        run.nested_events
            .send(UiEvent::SessionStarted {
                session_id: "s1".to_string(),
                resumed: false,
            })
            .expect("session started");
        let _ = run.nested_commands.recv().await.expect("prompt");
        run.nested_events
            .send(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new("review turn done"))),
            )))
            .expect("message");
        run.nested_events
            .send(UiEvent::PromptDone {
                stop_reason: StopReason::EndTurn,
                usage: None,
            })
            .expect("turn done");
        let _ = run.reports.recv().await.expect("coordinator report");

        loop {
            match next_visible_subagent_event(&mut run.ui_events).await {
                SubagentEvent::Finished { .. } => {
                    panic!("a retained coordinator emitted a terminal event between turns")
                }
                SubagentEvent::Status {
                    subagent_id,
                    message,
                    ..
                } if message.contains("session retained for automatic resume") => {
                    assert_eq!(subagent_id, run.subagent_id);
                    break;
                }
                _ => {}
            }
        }

        assert!(run.controller.cancel_and_wait().await);
        loop {
            if let SubagentEvent::Finished {
                subagent_id,
                outcome,
            } = next_visible_subagent_event(&mut run.ui_events).await
            {
                assert_eq!(subagent_id, run.subagent_id);
                assert_eq!(outcome, SubagentOutcome::Cancelled);
                break;
            }
        }
    }

    #[tokio::test]
    async fn prompt_done_waits_for_the_async_tool_to_finish() {
        let mut run = spawn_fake_run().await;
        run.nested_events
            .send(UiEvent::SessionStarted {
                session_id: "s1".to_string(),
                resumed: false,
            })
            .expect("session started");
        assert!(matches!(
            run.nested_commands.recv().await,
            Some(UiCommand::SendPrompt { .. })
        ));
        run.nested_events
            .send(UiEvent::SessionUpdate(SessionUpdate::ToolCall(
                agent_client_protocol::schema::v1::ToolCall::new("async", "background review")
                    .status(ToolCallStatus::InProgress),
            )))
            .expect("tool call");
        run.nested_events
            .send(UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new("candidate result"))),
            )))
            .expect("message");
        run.nested_events
            .send(UiEvent::PromptDone {
                stop_reason: StopReason::EndTurn,
                usage: None,
            })
            .expect("premature completion");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), run.reports.recv())
                .await
                .is_err(),
            "an active tool must keep the turn and report open"
        );

        let mut fields = ToolCallUpdateFields::default();
        fields.status = Some(ToolCallStatus::Completed);
        run.nested_events
            .send(UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(
                ToolCallUpdate::new("async", fields),
            )))
            .expect("terminal tool update");
        let report = tokio::time::timeout(Duration::from_secs(5), run.reports.recv())
            .await
            .expect("report after tool completion")
            .expect("report");
        assert_eq!(report.final_message, "candidate result");

        let released = run.registry.take(run.subagent_id).expect("retained run");
        let (respond, response) = oneshot::channel();
        released
            .control
            .send(WorkerRequest::Cancel { respond })
            .expect("release");
        let _ = response.await;
    }

    #[tokio::test]
    async fn non_retained_job_reaps_after_its_report() {
        let mut run = spawn_fake_run_with(Vec::new(), false).await;
        run.nested_events
            .send(UiEvent::SessionStarted {
                session_id: "s1".to_string(),
                resumed: false,
            })
            .expect("session started");
        let _ = run.nested_commands.recv().await.expect("prompt");
        run.nested_events
            .send(UiEvent::PromptDone {
                stop_reason: StopReason::EndTurn,
                usage: None,
            })
            .expect("turn done");
        let _ = tokio::time::timeout(Duration::from_secs(5), run.reports.recv())
            .await
            .expect("report")
            .expect("report value");
        run.controller.wait_until_absent(run.subagent_id).await;
        assert!(!run.registry.retained_ids().contains(&run.subagent_id));
    }

    #[tokio::test]
    async fn initial_programmatic_images_reach_the_nested_prompt() {
        let image = PromptImage {
            data_base64: "aW1hZ2U=".to_string(),
            mime_type: "image/png".to_string(),
            width: 2,
            height: 3,
        };
        let mut run = spawn_fake_run_with(vec![image.clone()], true).await;
        run.nested_events
            .send(UiEvent::SessionStarted {
                session_id: "s1".to_string(),
                resumed: false,
            })
            .expect("session started");
        let UiCommand::SendPrompt { images, .. } =
            run.nested_commands.recv().await.expect("prompt")
        else {
            panic!("expected prompt");
        };
        assert_eq!(images, vec![image]);
        let registered = run.registry.take(run.subagent_id).expect("running run");
        let (respond, response) = oneshot::channel();
        registered
            .control
            .send(WorkerRequest::Cancel { respond })
            .expect("cancel");
        assert!(matches!(
            run.nested_commands.recv().await,
            Some(UiCommand::CancelPrompt)
        ));
        run.nested_events
            .send(UiEvent::PromptDone {
                stop_reason: StopReason::Cancelled,
                usage: None,
            })
            .expect("settle cancel");
        let _ = response.await;
    }

    #[tokio::test]
    async fn cancelling_a_running_subagent_returns_the_tail_and_still_balances_the_report_account()
    {
        let mut run = spawn_fake_run().await;
        run.nested_events
            .send(UiEvent::SessionStarted {
                session_id: "s1".to_string(),
                resumed: false,
            })
            .expect("session started");
        assert!(matches!(
            run.nested_commands.recv().await,
            Some(UiCommand::SendPrompt { .. })
        ));
        run.nested_events
            .send(UiEvent::SessionUpdate(SessionUpdate::ToolCall(
                agent_client_protocol::schema::v1::ToolCall::new("t1", "half-finished work")
                    .status(ToolCallStatus::Completed),
            )))
            .expect("tool call");

        let registered = run.registry.take(run.subagent_id).expect("running run");
        let (respond, respond_rx) = oneshot::channel();
        registered
            .control
            .send(WorkerRequest::Cancel { respond })
            .expect("cancel");
        assert!(
            matches!(
                run.nested_commands.recv().await,
                Some(UiCommand::CancelPrompt)
            ),
            "cancelling a running subagent must interrupt its in-flight turn"
        );
        run.nested_events
            .send(UiEvent::PromptDone {
                stop_reason: StopReason::Cancelled,
                usage: None,
            })
            .expect("settle the cancelled turn");

        let result = tokio::time::timeout(Duration::from_secs(5), respond_rx)
            .await
            .expect("cancel settles")
            .expect("cancel result");
        assert!(result.cancelled_while_running);
        assert!(result.activity_log.contains("half-finished work"));
        assert!(result.workspace_delta.is_some());

        let report = tokio::time::timeout(Duration::from_secs(5), run.reports.recv())
            .await
            .expect("a cancelled run still reports so the account balances")
            .expect("report");
        assert_eq!(report.outcome, SubagentOutcome::Cancelled);

        run.controller.wait_until_absent(run.subagent_id).await;
        assert_eq!(run.controller.active_count().await, 0);
    }
}

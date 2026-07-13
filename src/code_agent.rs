//! One-shot nested ACP agent orchestration exposed to the primary agent as MCP.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    HttpHeader, McpServer, McpServerHttp, SessionUpdate, StopReason,
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
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::acp::{self, AcpRuntimeConfig, RuntimeAccessMode};
use crate::event::{
    CodeAgentEvent, CodeAgentOutcome, InternalMessage, InternalMessageKind, UiCommand, UiEvent,
    content_block_text,
};
use crate::loki;
use crate::workspace_snapshot::{WorkspaceDelta, WorkspaceSnapshot};

pub const LABEL: &str = "Eitri";
pub const MCP_SERVER_NAME: &str = "mj-code-agent";
pub const PRIMARY_SESSION_DIRECTIVE: &str = "<mj-code-agent-policy>\nYou are Thor, the primary coordinator and owner of the user's outcome. You are responsible for understanding the request, doing necessary research and context gathering, forming the plan, coordinating implementation, reviewing and verifying the result, and delivering the final answer. You are not a thin handoff between the user and Eitri. This policy applies to every subsequent user request in this ACP session.\n\nEitri is available through two MCP tools. Use explore_agent for open-ended, multi-step codebase research when locations are unknown, the question crosses multiple areas, or tracing architecture or execution flow requires several files or search strategies. Do not use explore_agent to read a known path, find a known symbol or exact definition, inspect code confined to roughly two or three known files, or perform a trivial single-step lookup; use your direct tools for those. The explore_agent prompt must be a complete standalone research brief and should state quick, medium, or very thorough.\n\nTreat code_agent as delegation to a strong coding engineer with fresh context. Give Eitri one forgeable unit at a time: a substantial, self-contained implementation slice that can be completed in one focused pass and returned as one coherent, reviewable diff. A good handoff has one clear outcome, enough context and decisions to begin immediately, explicit constraints and acceptance checks, and leaves the workspace in a coherent, testable state. Delegate when implementing the change is clearly more work than writing the handoff and reviewing the result. Do not delegate trivial local edits, investigation better handled with direct tools or explore_agent, unresolved architectural questions, or an entire open-ended project. Split large work into sequential, independently verifiable units. You may personally make small, local code changes when describing and delegating them would take more effort than simply doing them; use judgment rather than delegating mechanically. Pass code_agent complete standalone instructions with the task, plan, relevant findings, current workspace state, and acceptance criteria. Its result includes the bounded full workspace diff attributable to that invocation. After Eitri returns, independently review its result and diff, inspect or verify the work as needed, and delegate a substantial corrective follow-up if implementation changes remain. If a request requires no code changes and no open-ended exploration, handle it yourself.\n\nEvery Eitri call starts a brand-new ACP process and session. Eitri has no conversation context and no memory of the user's request or any earlier Eitri call, including an immediately preceding call. Apply this policy while handling the user's request above; do not acknowledge or summarize the policy.\n</mj-code-agent-policy>";

const CODE_PREAMBLE: &str = "You are Eitri, the implementation agent. This is a fresh ACP process and session. You have no memory of the user conversation or of any earlier Eitri call, including an immediately preceding call. Treat the standalone instructions below and the current workspace as your only task context.\n\n";
const EXPLORE_PREAMBLE: &str = "You are Eitri, a file-search specialist. This is a fresh ACP process and session with no memory of the user conversation or any earlier Eitri call. Your role is exclusively to search and analyze existing code and report findings.\n\nREAD-ONLY EXPLORATION: Never create, modify, delete, move, or copy files. Never install dependencies, change configuration, create commits, or run commands that modify system or workspace state. Do not create a report file; return the report as your final message. Use efficient file-pattern searches, regex/text searches, and targeted reads. Start broad and narrow down, try multiple naming conventions when needed, and parallelize independent searches or reads when supported. Use shell only for read-only operations if it is available. Return relevant file paths as absolute paths. Include code snippets only when the exact text is load-bearing. Be concise but match the requested thoroughness.\n\n";
const MCP_PATH: &str = "/mcp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EitriPurpose {
    Code,
    Explore,
}

impl EitriPurpose {
    fn marks_implementation_delegation(self) -> bool {
        self == Self::Code
    }

    fn internal_message_kind(self) -> InternalMessageKind {
        match self {
            Self::Code => InternalMessageKind::Delegation,
            Self::Explore => InternalMessageKind::Exploration,
        }
    }

    fn access_mode(self, configured: RuntimeAccessMode) -> RuntimeAccessMode {
        match self {
            Self::Code => configured,
            Self::Explore => RuntimeAccessMode::ReadOnly,
        }
    }

    fn standalone_prompt(self, task: &str) -> String {
        match self {
            Self::Code => format!("{CODE_PREAMBLE}{task}"),
            Self::Explore => {
                let thoroughness = exploration_thoroughness(task);
                format!(
                    "{EXPLORE_PREAMBLE}Thoroughness level: {thoroughness}.\n\nSearch request:\n{task}"
                )
            }
        }
    }

    fn loki_context(self, task: &str) -> String {
        match self {
            Self::Code => {
                format!("Eitri received this standalone implementation delegation:\n{task}")
            }
            Self::Explore => {
                format!("Eitri received this standalone read-only exploration request:\n{task}")
            }
        }
    }
}

fn exploration_thoroughness(prompt: &str) -> &'static str {
    let prompt = prompt.to_ascii_lowercase();
    if prompt.contains("very thorough") {
        "very thorough"
    } else if prompt.contains("quick") {
        "quick"
    } else {
        "medium"
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub display_label: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub agent_stderr: Option<PathBuf>,
    pub role_config: Option<acp::RuntimeRoleConfig>,
    pub loki: Option<loki::Handle>,
    pub implementation_handoff_counter: Option<Arc<AtomicUsize>>,
}

impl Config {
    #[cfg(test)]
    pub fn codex(agent_stderr: Option<PathBuf>, env: HashMap<String, String>) -> Self {
        Self {
            display_label: "Eitri · codex".to_string(),
            command: PathBuf::from("npx"),
            args: vec![
                "-y".to_string(),
                "@agentclientprotocol/codex-acp".to_string(),
            ],
            env,
            agent_stderr,
            role_config: None,
            loki: None,
            implementation_handoff_counter: None,
        }
    }

    pub fn council(
        command: PathBuf,
        args: Vec<String>,
        env: HashMap<String, String>,
        agent_stderr: Option<PathBuf>,
        model_id: String,
        model_value: String,
        loki: Option<loki::Handle>,
    ) -> Self {
        Self {
            display_label: format!("Eitri · {model_id}"),
            command,
            args,
            env,
            agent_stderr,
            role_config: Some(acp::RuntimeRoleConfig {
                label: LABEL.to_string(),
                model_value,
                force_high_reasoning: true,
            }),
            loki,
            implementation_handoff_counter: None,
        }
    }

    pub fn with_implementation_handoff_counter(mut self, counter: Arc<AtomicUsize>) -> Self {
        self.implementation_handoff_counter = Some(counter);
        self
    }
}

#[derive(Debug, Clone)]
pub struct RunContext {
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub fs_max_text_bytes: u64,
    pub access_mode: RuntimeAccessMode,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CodeAgentArgs {
    /// Complete, standalone coding task for the delegated agent.
    pub instructions: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExploreAgentArgs {
    /// Complete, standalone read-only research request for the delegated agent.
    pub prompt: String,
}

#[derive(Clone)]
struct McpHandler {
    config: Config,
    context: RunContext,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    controller: Controller,
    tools_listed: watch::Sender<bool>,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl McpHandler {
    fn new(
        config: Config,
        context: RunContext,
        ui_tx: mpsc::UnboundedSender<UiEvent>,
        controller: Controller,
        tools_listed: watch::Sender<bool>,
    ) -> Self {
        Self {
            config,
            context,
            ui_tx,
            controller,
            tools_listed,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "code_agent",
        description = "IMPLEMENTATION DELEGATE (EITRI). Treat this as delegation to a strong coding engineer with fresh context. Give Eitri one forgeable unit: a substantial, self-contained implementation slice that can be completed in one focused pass and returned as one coherent, reviewable diff. A good handoff has one clear outcome, enough context and decisions to begin immediately, explicit constraints and acceptance checks, and leaves the workspace coherent and testable. Delegate when implementation is clearly more work than writing the handoff and reviewing the result. Do NOT delegate trivial local edits, investigation better handled directly or with explore_agent, unresolved architectural questions, or an entire open-ended project; split large work into sequential, independently verifiable units. Thor owns research, planning, coordination, review, verification, and the final response, and should make small local changes directly when delegation would cost more effort. Every call starts a fresh ACP process/session with zero conversation or prior-call memory. Pass complete standalone instructions with the task, plan, relevant findings, current workspace state, and acceptance criteria. The result includes the bounded full workspace diff attributable to this invocation. Review Eitri's result and diff independently and call it again for substantial corrections."
    )]
    async fn code_agent(
        &self,
        Parameters(args): Parameters<CodeAgentArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        if args.instructions.trim().is_empty() {
            return Err(McpError::invalid_params(
                "instructions must not be empty",
                None,
            ));
        }
        if !self.controller.begin().await {
            return Ok(CallToolResult::error(vec![Content::text(
                "an Eitri run is already active",
            )]));
        }

        let result = run_boxed(
            self.config.clone(),
            self.context.clone(),
            args.instructions,
            EitriPurpose::Code,
            self.ui_tx.clone(),
            self.controller.clone(),
        )
        .await;
        let workspace_delta = result
            .workspace_delta
            .as_ref()
            .expect("code_agent always captures a workspace delta");
        Ok(match result.outcome {
            Ok(message) => CallToolResult::success(vec![Content::text(with_workspace_diff(
                &message,
                workspace_delta,
            ))]),
            Err(error) => CallToolResult::error(vec![Content::text(with_workspace_diff(
                &error.to_string(),
                workspace_delta,
            ))]),
        })
    }

    #[tool(
        name = "explore_agent",
        description = "READ-ONLY EXPLORATION DELEGATE (EITRI). Use this for open-ended, multi-step codebase research: finding files when locations are unknown, searching across multiple areas or naming conventions, tracing architecture or execution flow, or answering questions that require several files or search strategies. Do NOT use it to read a known path, find a known symbol or exact definition, inspect code confined to roughly 2-3 known files, or perform a trivial single-step lookup; use your direct tools instead. The prompt must be a complete standalone research brief and should state the desired thoroughness: quick, medium, or very thorough. Every call starts a fresh ACP process/session with zero conversation or prior-call memory and returns one final research report."
    )]
    async fn explore_agent(
        &self,
        Parameters(args): Parameters<ExploreAgentArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let prompt = args.prompt.trim();
        if prompt.is_empty() {
            return Err(McpError::invalid_params("prompt must not be empty", None));
        }
        if !self.controller.begin().await {
            return Ok(CallToolResult::error(vec![Content::text(
                "an Eitri run is already active",
            )]));
        }

        let result = run_boxed(
            self.config.clone(),
            self.context.clone(),
            prompt.to_string(),
            EitriPurpose::Explore,
            self.ui_tx.clone(),
            self.controller.clone(),
        )
        .await;
        Ok(match result.outcome {
            Ok(message) => CallToolResult::success(vec![Content::text(message)]),
            Err(error) => CallToolResult::error(vec![Content::text(error.to_string())]),
        })
    }
}

impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mj-code-agent", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "EITRI DELEGATION POLICY: Use explore_agent only for open-ended, multi-step codebase research across unknown or multiple locations; use direct tools for known paths, known symbols, roughly 2-3 known files, and trivial lookups. Give code_agent one forgeable unit at a time: a substantial, self-contained implementation slice with one clear outcome that can be completed in one focused pass and returned as one coherent, reviewable diff. Do not delegate trivial edits, unresolved architecture, or an entire open-ended project; split large work into independently verifiable units. Thor retains planning, coordination, review, verification, and the final answer. Every Eitri call is a fresh ACP process/session and needs a complete standalone prompt.",
            )
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = std::result::Result<ListToolsResult, McpError>> + Send + '_ {
        let _ = self.tools_listed.send(true);
        std::future::ready(Ok(ListToolsResult::with_all_items(
            self.tool_router.list_all(),
        )))
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
        self.tool_router.get(name).cloned()
    }
}

/// In-process, loopback-only MCP endpoint advertised to the primary ACP agent.
/// Dropping it cancels the listener and every open MCP session.
pub struct HttpServer {
    advertised: McpServer,
    tools_listed: watch::Receiver<bool>,
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
        let mut token_bytes = [0_u8; 32];
        getrandom::fill(&mut token_bytes)
            .map_err(|error| anyhow!("generate code-agent MCP bearer token: {error}"))?;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token_bytes);
        let authorization = format!("Bearer {token}");

        let (tools_listed_tx, tools_listed) = watch::channel(false);
        let handler = McpHandler::new(config, context, ui_tx, controller, tools_listed_tx);
        let cancellation = CancellationToken::new();
        let mut server_config = StreamableHttpServerConfig::default();
        server_config.cancellation_token = cancellation.clone();
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(LocalSessionManager::default()),
            server_config,
        );
        let protected = axum::Router::new().nest_service(MCP_PATH, service).layer(
            axum::middleware::from_fn_with_state(authorization.clone(), require_bearer),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind code-agent MCP listener")?;
        let addr = listener
            .local_addr()
            .context("read code-agent MCP listener address")?;
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, protected)
                .with_graceful_shutdown(task_cancellation.cancelled_owned())
                .await
            {
                tracing::warn!("code-agent MCP listener stopped: {error}");
            }
        });
        let advertised = McpServer::Http(
            McpServerHttp::new(MCP_SERVER_NAME, format!("http://{addr}{MCP_PATH}"))
                .headers(vec![HttpHeader::new("Authorization", authorization)]),
        );
        Ok(Self {
            advertised,
            tools_listed,
            cancellation,
            task,
        })
    }

    pub fn advertised(&self) -> &McpServer {
        &self.advertised
    }

    pub async fn wait_until_tools_listed(&self, timeout: Duration) -> Result<()> {
        let mut tools_listed = self.tools_listed.clone();
        if *tools_listed.borrow() {
            return Ok(());
        }
        tokio::time::timeout(timeout, tools_listed.changed())
            .await
            .map_err(|_| anyhow!("primary agent timed out loading the Eitri MCP tools"))?
            .map_err(|_| anyhow!("code-agent MCP server closed before tools/list"))?;
        Ok(())
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

#[derive(Debug, Default)]
enum ActiveRun {
    #[default]
    Idle,
    Starting {
        cancel_requested: bool,
        shutdown_requested: bool,
    },
    Running {
        commands: mpsc::UnboundedSender<UiCommand>,
    },
}

/// Coordinates the one allowed Eitri foreground run. Thor remains suspended in
/// its MCP request while UI cancellation is routed exclusively to this lane.
#[derive(Debug, Clone, Default)]
pub struct Controller {
    state: Arc<Mutex<ActiveRun>>,
}

impl Controller {
    pub async fn begin(&self) -> bool {
        let mut state = self.state.lock().await;
        if !matches!(*state, ActiveRun::Idle) {
            return false;
        }
        *state = ActiveRun::Starting {
            cancel_requested: false,
            shutdown_requested: false,
        };
        true
    }

    async fn attach(&self, commands: mpsc::UnboundedSender<UiCommand>) {
        let mut state = self.state.lock().await;
        let (cancel_requested, shutdown_requested) = match *state {
            ActiveRun::Starting {
                cancel_requested,
                shutdown_requested,
            } => (cancel_requested, shutdown_requested),
            _ => (false, false),
        };
        *state = ActiveRun::Running {
            commands: commands.clone(),
        };
        if shutdown_requested {
            let _ = commands.send(UiCommand::Shutdown);
        } else if cancel_requested {
            let _ = commands.send(UiCommand::CancelPrompt);
        }
    }

    pub async fn cancel(&self) -> bool {
        let mut state = self.state.lock().await;
        match &mut *state {
            ActiveRun::Idle => false,
            ActiveRun::Starting {
                cancel_requested, ..
            } => {
                *cancel_requested = true;
                true
            }
            ActiveRun::Running { commands } => {
                let _ = commands.send(UiCommand::CancelPrompt);
                true
            }
        }
    }

    pub async fn shutdown(&self) -> bool {
        let mut state = self.state.lock().await;
        match &mut *state {
            ActiveRun::Idle => false,
            ActiveRun::Starting {
                shutdown_requested, ..
            } => {
                *shutdown_requested = true;
                true
            }
            ActiveRun::Running { commands } => {
                let _ = commands.send(UiCommand::Shutdown);
                true
            }
        }
    }

    pub async fn finish(&self) {
        *self.state.lock().await = ActiveRun::Idle;
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
            bail!("Eitri completed without a final message");
        }
        Ok(self.last.clone())
    }
}

struct EitriRunResult {
    outcome: Result<String>,
    workspace_delta: Option<WorkspaceDelta>,
}

fn run_boxed(
    config: Config,
    context: RunContext,
    task: String,
    purpose: EitriPurpose,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    controller: Controller,
) -> futures::future::BoxFuture<'static, EitriRunResult> {
    Box::pin(run(config, context, task, purpose, ui_tx, controller))
}

async fn run(
    config: Config,
    context: RunContext,
    task: String,
    purpose: EitriPurpose,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    controller: Controller,
) -> EitriRunResult {
    if purpose.marks_implementation_delegation()
        && let Some(counter) = config.implementation_handoff_counter.as_ref()
    {
        counter.fetch_add(1, Ordering::AcqRel);
    }
    let standalone_prompt = purpose.standalone_prompt(&task);
    let display_label = config.display_label.clone();
    let _ = ui_tx.send(UiEvent::InternalMessage(InternalMessage {
        source: "Thor".to_string(),
        target: LABEL.to_string(),
        kind: purpose.internal_message_kind(),
        text: task.clone(),
    }));
    let _ = ui_tx.send(UiEvent::CodeAgent(CodeAgentEvent::Started {
        label: display_label,
    }));

    let invocation_snapshot = if purpose.marks_implementation_delegation() {
        let mut workspace_roots = Vec::with_capacity(1 + context.additional_directories.len());
        workspace_roots.push(context.cwd.clone());
        workspace_roots.extend(context.additional_directories.iter().cloned());
        Some(WorkspaceSnapshot::capture(&workspace_roots).await)
    } else {
        None
    };

    let (nested_event_tx, mut nested_event_rx) = mpsc::unbounded_channel();
    let (nested_cmd_tx, nested_cmd_rx) = mpsc::unbounded_channel();
    controller.attach(nested_cmd_tx.clone()).await;

    let loki = config.loki.clone();
    let runtime_config = AcpRuntimeConfig {
        command: config.command,
        args: config.args,
        cwd: context.cwd,
        additional_directories: context.additional_directories,
        mcp_servers: Vec::new(),
        resume_session: None,
        env: config.env,
        agent_stderr: config.agent_stderr,
        fs_max_text_bytes: context.fs_max_text_bytes,
        access_mode: purpose.access_mode(context.access_mode),
        agent_source_id: None,
        config_path: None,
        saved_session_config: HashMap::new(),
        role_config: config.role_config,
        code_agent: None,
    };
    let mut runtime = tokio::spawn(acp::run(runtime_config, nested_event_tx, nested_cmd_rx));

    let mut prompt_sent = false;
    let mut collector = AgentMessageCollector::new();
    let epoch = loki.as_ref().map_or(0, loki::Handle::current_epoch);
    if epoch > 0
        && let Some(reviewer) = loki.as_ref()
    {
        reviewer.begin_eitri(epoch, purpose.loki_context(&task));
    }
    let mut decisions = loki.as_ref().map(loki::Handle::subscribe);
    let mut tracker = loki::BoundaryTracker::default();
    let mut pending_reviews = std::collections::HashSet::new();
    let mut completed: Option<Result<String>> = None;
    let mut intervention = loki::DeferredIntervention::default();
    let result = loop {
        tokio::select! {
            joined = &mut runtime => {
                break match joined {
                    Ok(Ok(())) => Err(anyhow!("Eitri runtime closed before completing")),
                    Ok(Err(error)) => Err(error).context("Eitri runtime"),
                    Err(error) => Err(anyhow!("Eitri task failed: {error}")),
                };
            }
            event = nested_event_rx.recv() => {
                let Some(event) = event else {
                    break Err(anyhow!("Eitri event stream closed before completing"));
                };
                let boundary = (epoch > 0).then(|| tracker.observe(&event)).flatten();
                let boundary_observed = boundary.is_some();
                let target_completed = matches!(
                    &event,
                    UiEvent::PromptDone { .. }
                        | UiEvent::PromptFailed { .. }
                        | UiEvent::SessionForkFailed { .. }
                        | UiEvent::Fatal(_)
                );
                let interrupting = boundary_observed
                    && !target_completed
                    && intervention.interrupt_at_boundary();
                if interrupting {
                    let _ = nested_cmd_tx.send(UiCommand::CancelPrompt);
                }
                if let Some(boundary) = boundary
                    && !interrupting
                    && !(target_completed && intervention.is_pending())
                    && let Some(reviewer) = loki.as_ref()
                    && let Some(id) = reviewer
                        .observe(epoch, loki::Target::Eitri, boundary)
                        .await
                {
                    pending_reviews.insert(id);
                }
                match event {
                    UiEvent::Connected { .. } => {}
                    UiEvent::SessionStarted { .. } if !prompt_sent => {
                        prompt_sent = true;
                        if nested_cmd_tx
                            .send(UiCommand::SendPrompt {
                                text: standalone_prompt.clone(),
                                images: Vec::new(),
                            })
                            .is_err()
                        {
                            break Err(anyhow!("send prompt to Eitri"));
                        }
                    }
                    UiEvent::SessionStarted { .. } | UiEvent::SessionConfigOptions { .. } => {}
                    UiEvent::SessionUpdate(update) => {
                        collector.observe(&update);
                        let _ = ui_tx.send(UiEvent::CodeAgent(CodeAgentEvent::SessionUpdate(update)));
                    }
                    UiEvent::TerminalOutput(snapshot) => {
                        let _ = ui_tx.send(UiEvent::CodeAgent(CodeAgentEvent::TerminalOutput(snapshot)));
                    }
                    UiEvent::PermissionRequest(prompt) => {
                        let _ = ui_tx.send(UiEvent::CodeAgent(CodeAgentEvent::PermissionRequest(prompt)));
                    }
                    UiEvent::ElicitationRequest(prompt) => {
                        let _ = ui_tx.send(UiEvent::CodeAgent(CodeAgentEvent::ElicitationRequest(prompt)));
                    }
                    UiEvent::CancelPendingPermissions => {
                        let _ = ui_tx.send(UiEvent::CodeAgent(CodeAgentEvent::CancelPendingPermissions));
                    }
                    UiEvent::Info(message) | UiEvent::Warning(message) => {
                        let _ = ui_tx.send(UiEvent::CodeAgent(CodeAgentEvent::Status(message)));
                    }
                    UiEvent::PromptDone { stop_reason, .. } => {
                        if matches!(stop_reason, StopReason::Cancelled) {
                            if intervention.is_pending()
                                && !intervention.cancellation_was_requested()
                            {
                                // A user cancellation wins if Loki had not yet
                                // reached the deferred interruption boundary.
                                intervention.clear();
                                pending_reviews.clear();
                                break Err(anyhow!("Eitri cancelled"));
                            }
                            if let Some(critique) = intervention.take() {
                                collector = AgentMessageCollector::new();
                                tracker.reset_attempt();
                                pending_reviews.clear();
                                completed = None;
                                let continuation = continuation_prompt(purpose, &critique);
                                emit_continuation(&ui_tx, &critique);
                                if nested_cmd_tx.send(UiCommand::SendPrompt { text: continuation, images: Vec::new() }).is_err() {
                                    break Err(anyhow!("re-prompt Eitri after Loki intervention"));
                                }
                                continue;
                            }
                            break Err(anyhow!("Eitri cancelled"));
                        }
                        if let Some(critique) = intervention.take() {
                            collector = AgentMessageCollector::new();
                            tracker.reset_attempt();
                            pending_reviews.clear();
                            completed = None;
                            let continuation = continuation_prompt(purpose, &critique);
                            emit_continuation(&ui_tx, &critique);
                            if nested_cmd_tx.send(UiCommand::SendPrompt { text: continuation, images: Vec::new() }).is_err() {
                                break Err(anyhow!("re-prompt Eitri after Loki intervention"));
                            }
                            continue;
                        }
                        completed = Some(collector.finish());
                        if pending_reviews.is_empty() {
                            break completed.take().expect("completion stored");
                        }
                    }
                    UiEvent::PromptFailed { message }
                    | UiEvent::SessionForkFailed { message }
                    | UiEvent::Fatal(message) => {
                        if let Some(critique) = intervention.take() {
                            collector = AgentMessageCollector::new();
                            tracker.reset_attempt();
                            pending_reviews.clear();
                            completed = None;
                            let continuation = continuation_prompt(purpose, &critique);
                            emit_continuation(&ui_tx, &critique);
                            if nested_cmd_tx.send(UiCommand::SendPrompt { text: continuation, images: Vec::new() }).is_err() {
                                break Err(anyhow!("re-prompt Eitri after Loki intervention"));
                            }
                            continue;
                        }
                        completed = Some(Err(anyhow!(message)));
                        if pending_reviews.is_empty() {
                            break completed.take().expect("completion stored");
                        }
                    }
                    UiEvent::ClaudeUsage(_)
                    | UiEvent::RemotePermissionDecision { .. }
                    | UiEvent::LokiActivity(_)
                    | UiEvent::InternalMessage(_) => {}
                    UiEvent::CodeAgent(_) => {
                        break Err(anyhow!("Eitri attempted recursive delegation"));
                    }
                }
            }
            decision = async {
                match decisions.as_mut() {
                    Some(rx) => rx.recv().await.ok(),
                    None => std::future::pending().await,
                }
            } => {
                let Some(decision) = decision else { continue; };
                if decision.epoch != epoch || decision.target != loki::Target::Eitri || !pending_reviews.remove(&decision.id) {
                    continue;
                }
                match decision.verdict {
                    loki::Verdict::Intervention(critique) => {
                        intervention.push(decision.id, critique);
                        if completed.is_some() {
                            completed = None;
                            collector = AgentMessageCollector::new();
                            tracker.reset_attempt();
                            pending_reviews.clear();
                            let critique = intervention.take().expect("intervention queued");
                            let continuation = continuation_prompt(purpose, &critique);
                            emit_continuation(&ui_tx, &critique);
                            if nested_cmd_tx.send(UiCommand::SendPrompt { text: continuation, images: Vec::new() }).is_err() {
                                break Err(anyhow!("re-prompt Eitri after Loki intervention"));
                            }
                        }
                    }
                    loki::Verdict::NoIntervention => {}
                    loki::Verdict::Failed(message) => {
                        tracing::debug!("Loki Eitri review failed open: {message}");
                    }
                }
                if pending_reviews.is_empty() && !intervention.is_pending()
                    && let Some(result) = completed.take()
                {
                    break result;
                }
            }
        }
    };

    let _ = nested_cmd_tx.send(UiCommand::Shutdown);
    if !runtime.is_finished()
        && tokio::time::timeout(Duration::from_secs(2), &mut runtime)
            .await
            .is_err()
    {
        runtime.abort();
        let _ = runtime.await;
    }
    controller.finish().await;
    let workspace_delta = match invocation_snapshot {
        Some(snapshot) => Some(snapshot.delta().await),
        None => None,
    };

    let outcome = match &result {
        Ok(_) => CodeAgentOutcome::Completed,
        Err(error) if error.to_string().contains("cancelled") => CodeAgentOutcome::Cancelled,
        Err(error) => CodeAgentOutcome::Failed(error.to_string()),
    };
    let _ = ui_tx.send(UiEvent::CodeAgent(CodeAgentEvent::Finished { outcome }));
    EitriRunResult {
        outcome: result,
        workspace_delta,
    }
}

fn with_workspace_diff(message: &str, delta: &WorkspaceDelta) -> String {
    let diff = delta.review_patch().unwrap_or_else(|| delta.receipt());
    let mut result = format!(
        "{message}\n\n<workspace_diff scope=\"eitri-invocation\" authored_by=\"Eitri\">\n{diff}\n</workspace_diff>"
    );
    if delta.changed() {
        result.push_str("\n\nYou should review Eitri's work now.");
    }
    result
}

fn continuation_prompt(purpose: EitriPurpose, critique: &str) -> String {
    let activity = match purpose {
        EitriPurpose::Code => "implementation",
        EitriPurpose::Explore => "read-only exploration",
    };
    format!(
        "<advisory guidance=\"weigh, don't blindly obey\">\n{critique}\n</advisory>\n\nContinue the interrupted {activity} turn. Address the material advice, then finish the existing task. Please continue from where you left off."
    )
}

fn emit_continuation(ui_tx: &mpsc::UnboundedSender<UiEvent>, text: &str) {
    let _ = ui_tx.send(UiEvent::InternalMessage(InternalMessage {
        source: "Loki".to_string(),
        target: LABEL.to_string(),
        kind: InternalMessageKind::Continuation,
        text: text.to_string(),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, TextContent};

    fn test_handler() -> McpHandler {
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel();
        let (tools_listed, _tools_listed_rx) = watch::channel(false);
        McpHandler::new(
            Config::codex(None, HashMap::new()),
            RunContext {
                cwd: PathBuf::from("/tmp/workspace"),
                additional_directories: Vec::new(),
                fs_max_text_bytes: 1024,
                access_mode: RuntimeAccessMode::Full,
            },
            ui_tx,
            Controller::default(),
            tools_listed,
        )
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
    }

    #[test]
    fn collector_rejects_message_less_completion() {
        let collector = AgentMessageCollector::new();
        assert!(collector.finish().is_err());
    }

    #[tokio::test]
    async fn controller_rejects_concurrent_runs_and_resets() {
        let controller = Controller::default();
        assert!(controller.begin().await);
        assert!(!controller.begin().await);
        assert!(controller.cancel().await);
        controller.finish().await;
        assert!(controller.begin().await);
    }

    #[tokio::test]
    async fn shutdown_requested_while_starting_reaches_nested_runtime() {
        let controller = Controller::default();
        assert!(controller.begin().await);
        assert!(controller.shutdown().await);
        let (commands, mut receiver) = mpsc::unbounded_channel();
        controller.attach(commands).await;
        assert!(matches!(receiver.recv().await, Some(UiCommand::Shutdown)));
    }

    #[test]
    fn tool_arguments_are_strict() {
        let parsed: CodeAgentArgs =
            serde_json::from_str(r#"{"instructions":"fix it"}"#).expect("valid arguments");
        assert_eq!(parsed.instructions, "fix it");
        assert!(
            serde_json::from_str::<CodeAgentArgs>(r#"{"instructions":"fix it","unexpected":true}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<CodeAgentArgs>("{}").is_err());

        let parsed: ExploreAgentArgs =
            serde_json::from_str(r#"{"prompt":"very thorough: trace it"}"#)
                .expect("valid explore arguments");
        assert_eq!(parsed.prompt, "very thorough: trace it");
        assert!(
            serde_json::from_str::<ExploreAgentArgs>(
                r#"{"prompt":"trace it","instructions":"wrong field"}"#
            )
            .is_err()
        );
        assert!(serde_json::from_str::<ExploreAgentArgs>("{}").is_err());
    }

    #[test]
    fn tools_are_model_visible_and_direct_each_eitri_purpose() {
        let tools = test_handler().tool_router.list_all();
        assert_eq!(tools.len(), 2);
        let tool = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "code_agent")
            .map(|tool| serde_json::to_value(tool).expect("serialize tool"))
            .expect("code_agent");
        assert_eq!(tool["name"], "code_agent");
        let description = tool["description"].as_str().expect("description");
        assert!(description.contains("IMPLEMENTATION DELEGATE"));
        assert!(description.contains("Thor owns research, planning"));
        assert!(description.contains("one forgeable unit"));
        assert!(description.contains("substantial, self-contained implementation slice"));
        assert!(description.contains("one focused pass"));
        assert!(description.contains("one coherent, reviewable diff"));
        assert!(description.contains("one clear outcome"));
        assert!(description.contains("explicit constraints and acceptance checks"));
        assert!(description.contains("implementation is clearly more work"));
        assert!(description.contains("Do NOT delegate trivial local edits"));
        assert!(description.contains("unresolved architectural questions"));
        assert!(description.contains("entire open-ended project"));
        assert!(description.contains("independently verifiable units"));
        assert!(description.contains("small local changes directly"));
        assert!(description.contains("fresh ACP process/session"));
        assert!(description.contains("zero conversation or prior-call memory"));
        assert!(description.contains("bounded full workspace diff"));
        assert!(description.contains("Review Eitri's result and diff independently"));
        assert_eq!(
            tool["inputSchema"]["required"],
            serde_json::json!(["instructions"])
        );

        let explore = tools
            .iter()
            .find(|tool| tool.name.as_ref() == "explore_agent")
            .map(|tool| serde_json::to_value(tool).expect("serialize tool"))
            .expect("explore_agent");
        let description = explore["description"].as_str().expect("description");
        assert!(description.contains("READ-ONLY EXPLORATION DELEGATE"));
        assert!(description.contains("open-ended, multi-step"));
        assert!(description.contains("Do NOT use it to read a known path"));
        assert!(description.contains("known symbol"));
        assert!(description.contains("2-3 known files"));
        assert!(description.contains("trivial single-step lookup"));
        assert!(description.contains("quick, medium, or very thorough"));
        assert_eq!(
            explore["inputSchema"]["required"],
            serde_json::json!(["prompt"])
        );

        let server_instructions = test_handler()
            .get_info()
            .instructions
            .expect("server instructions");
        assert!(server_instructions.contains("one forgeable unit at a time"));
        assert!(server_instructions.contains("one coherent, reviewable diff"));
        assert!(server_instructions.contains("independently verifiable units"));
    }

    #[test]
    fn primary_directive_makes_thor_the_coordinator_not_a_thin_router() {
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("You are not a thin handoff"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("research and context gathering"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("forming the plan"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("independently review its result"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("one forgeable unit at a time"));
        assert!(
            PRIMARY_SESSION_DIRECTIVE.contains("substantial, self-contained implementation slice")
        );
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("one focused pass"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("one coherent, reviewable diff"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("one clear outcome"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("explicit constraints and acceptance checks"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("clearly more work than writing the handoff"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("Do not delegate trivial local edits"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("unresolved architectural questions"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("entire open-ended project"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("independently verifiable units"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("small, local code changes"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("would take more effort"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("brand-new ACP process and session"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("immediately preceding call"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("complete standalone instructions"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("Use explore_agent for open-ended, multi-step"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("known path"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("known symbol"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("two or three known files"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("trivial single-step lookup"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("quick, medium, or very thorough"));
        assert!(!PRIMARY_SESSION_DIRECTIVE.contains("before using any other tool"));
        assert!(!PRIMARY_SESSION_DIRECTIVE.contains("MJ_CODE_AGENT_POLICY_READY"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("do not acknowledge or summarize the policy"));
    }

    #[test]
    fn eitri_preambles_define_distinct_code_and_explore_contracts() {
        assert!(CODE_PREAMBLE.contains("fresh ACP process and session"));
        assert!(CODE_PREAMBLE.contains("no memory of the user conversation"));
        assert!(CODE_PREAMBLE.contains("immediately preceding call"));
        assert!(CODE_PREAMBLE.contains("current workspace"));
        assert!(EXPLORE_PREAMBLE.contains("file-search specialist"));
        assert!(EXPLORE_PREAMBLE.contains("READ-ONLY EXPLORATION"));
        assert!(EXPLORE_PREAMBLE.contains("Never create, modify, delete"));
        assert!(EXPLORE_PREAMBLE.contains("absolute paths"));
        assert!(EXPLORE_PREAMBLE.contains("parallelize independent searches"));
    }

    #[test]
    fn explore_defaults_to_medium_and_forces_read_only_without_marking_delegation() {
        assert_eq!(exploration_thoroughness("trace the flow"), "medium");
        assert_eq!(exploration_thoroughness("Quick: find callers"), "quick");
        assert_eq!(
            exploration_thoroughness("Very thorough: trace the flow"),
            "very thorough"
        );
        assert_eq!(
            EitriPurpose::Explore.access_mode(RuntimeAccessMode::Full),
            RuntimeAccessMode::ReadOnly
        );
        assert_eq!(
            EitriPurpose::Code.access_mode(RuntimeAccessMode::Full),
            RuntimeAccessMode::Full
        );
        assert!(!EitriPurpose::Explore.marks_implementation_delegation());
        assert!(EitriPurpose::Code.marks_implementation_delegation());
        assert!(
            EitriPurpose::Explore
                .standalone_prompt("trace it")
                .contains("Thoroughness level: medium")
        );
    }

    #[test]
    fn loki_continuation_prompt_keeps_extra_resume_instruction_hidden() {
        let critique = "Would it be better to verify the fallback first?";
        let prompt = continuation_prompt(EitriPurpose::Code, critique);
        assert!(prompt.contains(critique));
        assert!(prompt.contains("Please continue from where you left off."));

        let (tx, mut rx) = mpsc::unbounded_channel();
        emit_continuation(&tx, critique);
        let event = rx.try_recv().expect("continuation event");
        let UiEvent::InternalMessage(message) = event else {
            panic!("expected internal message");
        };
        assert_eq!(message.text, critique);
        assert!(
            !message
                .text
                .contains("Please continue from where you left off.")
        );
    }
}

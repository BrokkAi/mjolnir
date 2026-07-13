//! Persistent read-only council advisor. Thor and Eitri stream hidden transcript
//! deltas into one Loki ACP session; only calls to Loki's `advise` MCP tool are
//! projected back into the visible transcript and steering machinery.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

use agent_client_protocol::schema::v1::{HttpHeader, McpServer, McpServerHttp};
use anyhow::{Context, Result, anyhow};
use axum::extract::{Request as HttpRequest, State};
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
use tokio::sync::{Mutex, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::acp::{RuntimeAccessMode, RuntimeRoleConfig};
use crate::council::ResolvedRole;
use crate::event::{LokiActivity, LokiIdentity, UiEvent};
use crate::ragnarok::{AgentHandle, Launch, TurnEvent};

const REVIEW_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_CONTEXT_BYTES: usize = 96 * 1024;
const MAX_DELTA_ITEM_BYTES: usize = 16 * 1024;
const MCP_PATH: &str = "/mcp";
const MCP_SERVER_NAME: &str = "mj-loki-advisor";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AdviseArgs {
    /// One concrete, material, actionable suggestion for the watched agent.
    note: String,
}

#[derive(Debug, Clone)]
struct ActiveAdvice {
    id: u64,
    epoch: u64,
    target: Target,
    accepted: bool,
}

#[derive(Debug, Clone, Default)]
struct AdviceSlot {
    active: Arc<Mutex<Option<ActiveAdvice>>>,
}

impl AdviceSlot {
    async fn begin(&self, id: u64, epoch: u64, target: Target) {
        *self.active.lock().await = Some(ActiveAdvice {
            id,
            epoch,
            target,
            accepted: false,
        });
    }

    async fn accept(&self, note: String) -> std::result::Result<Decision, &'static str> {
        let mut active = self.active.lock().await;
        let Some(active) = active.as_mut() else {
            return Err("no advisor update is active");
        };
        if active.accepted {
            return Err("only one advice note is allowed per advisor update");
        }
        active.accepted = true;
        Ok(Decision {
            id: active.id,
            epoch: active.epoch,
            target: active.target,
            verdict: Verdict::Intervention(note),
        })
    }

    async fn finish(&self, id: u64) -> bool {
        let mut active = self.active.lock().await;
        if active.as_ref().is_some_and(|active| active.id == id) {
            return active.take().is_some_and(|active| active.accepted);
        }
        false
    }
}

#[derive(Clone)]
struct McpHandler {
    advice: AdviceSlot,
    decisions: broadcast::Sender<Decision>,
    tools_listed: watch::Sender<bool>,
    tool_router: ToolRouter<Self>,
}

#[tool_router(router = tool_router)]
impl McpHandler {
    fn new(
        advice: AdviceSlot,
        decisions: broadcast::Sender<Decision>,
        tools_listed: watch::Sender<bool>,
    ) -> Self {
        Self {
            advice,
            decisions,
            tools_listed,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        name = "advise",
        description = "Send one material advisory suggestion to the agent you are currently watching. Calling this tool forces Mjolnir to cancel that agent at the next safe step boundary and re-prompt it with your note. Use it only for a material correctness, safety, scope, or strategy concern; otherwise do not call it. Phrase most notes as questions, such as 'Would it be better to...' or 'Have you thought about...?'"
    )]
    async fn advise(
        &self,
        Parameters(args): Parameters<AdviseArgs>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let note = args.note.trim();
        if note.is_empty() {
            return Err(McpError::invalid_params("note must not be empty", None));
        }
        match self.advice.accept(note.to_string()).await {
            Ok(decision) => {
                let _ = self.decisions.send(decision);
                Ok(CallToolResult::success(vec![Content::text(
                    "Advice queued for the watched agent.",
                )]))
            }
            Err(message) => Ok(CallToolResult::error(vec![Content::text(message)])),
        }
    }
}

impl ServerHandler for McpHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "mj-loki-advisor",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "You are a pure advisor. The advise tool is your only channel back to the watched agent. Any call forces cancellation and re-prompting, so stay silent unless a material suggestion is necessary. Prefer advice framed as questions rather than corrections.",
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

struct HttpServer {
    advertised: McpServer,
    tools_listed: watch::Receiver<bool>,
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

impl HttpServer {
    async fn start(advice: AdviceSlot, decisions: broadcast::Sender<Decision>) -> Result<Self> {
        let mut token_bytes = [0_u8; 32];
        getrandom::fill(&mut token_bytes)
            .map_err(|error| anyhow!("generate Loki advisor MCP bearer token: {error}"))?;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token_bytes);
        let authorization = format!("Bearer {token}");
        let (tools_listed_tx, tools_listed) = watch::channel(false);
        let handler = McpHandler::new(advice, decisions, tools_listed_tx);
        let cancellation = CancellationToken::new();
        let mut config = StreamableHttpServerConfig::default();
        config.cancellation_token = cancellation.clone();
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::new(LocalSessionManager::default()),
            config,
        );
        let protected = axum::Router::new().nest_service(MCP_PATH, service).layer(
            axum::middleware::from_fn_with_state(authorization.clone(), require_bearer),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind Loki advisor MCP listener")?;
        let addr = listener
            .local_addr()
            .context("read Loki advisor MCP listener address")?;
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, protected)
                .with_graceful_shutdown(task_cancellation.cancelled_owned())
                .await
            {
                tracing::warn!("Loki advisor MCP listener stopped: {error}");
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

    async fn wait_until_tools_listed(&self) -> Result<()> {
        let mut listed = self.tools_listed.clone();
        if *listed.borrow() {
            return Ok(());
        }
        tokio::time::timeout(Duration::from_secs(30), listed.changed())
            .await
            .map_err(|_| anyhow!("Loki timed out loading the advise MCP tool"))?
            .map_err(|_| anyhow!("Loki advisor MCP server closed before tools/list"))?;
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
    request: HttpRequest,
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

/// Loki critiques waiting to be delivered to a target at a safe step boundary.
///
/// Review decisions arrive asynchronously, often after the target has already
/// started its next operation. Keep them ordered by review id and request at
/// most one cancellation when the next boundary is observed.
#[derive(Default)]
pub struct DeferredIntervention {
    critiques: BTreeMap<u64, String>,
    cancel_requested: bool,
}

impl DeferredIntervention {
    pub fn push(&mut self, id: u64, critique: String) {
        self.critiques.insert(id, critique);
    }

    pub fn is_pending(&self) -> bool {
        !self.critiques.is_empty()
    }

    /// Mark the next observed non-terminal step boundary for interruption.
    pub fn interrupt_at_boundary(&mut self) -> bool {
        if self.critiques.is_empty() || self.cancel_requested {
            return false;
        }
        self.cancel_requested = true;
        true
    }

    pub fn cancellation_was_requested(&self) -> bool {
        self.cancel_requested
    }

    /// Drain queued critiques in observation order for one continuation prompt.
    pub fn take(&mut self) -> Option<String> {
        if self.critiques.is_empty() {
            return None;
        }
        self.cancel_requested = false;
        let critiques = std::mem::take(&mut self.critiques);
        Some(
            critiques
                .into_values()
                .enumerate()
                .map(|(index, critique)| {
                    if index == 0 {
                        critique
                    } else {
                        format!("Additional Loki critique: {critique}")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    }

    pub fn clear(&mut self) {
        self.critiques.clear();
        self.cancel_requested = false;
    }
}

#[derive(Default)]
pub struct BoundaryTracker {
    trajectory: String,
    final_message: String,
    segment: String,
    lane: Option<SegmentLane>,
    tools: HashMap<String, agent_client_protocol::schema::v1::ToolCall>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SegmentLane {
    Message,
    Thought,
}

impl BoundaryTracker {
    pub fn observe(&mut self, event: &UiEvent) -> Option<String> {
        use agent_client_protocol::schema::v1::{SessionUpdate, ToolCall, ToolCallStatus};
        let flush = |this: &mut Self| {
            let lane = this.lane.take()?;
            if this.segment.trim().is_empty() {
                this.segment.clear();
                return None;
            }
            let kind = match lane {
                SegmentLane::Message => "message",
                SegmentLane::Thought => "thinking",
            };
            Some(format!("{kind}:\n{}", std::mem::take(&mut this.segment)))
        };
        let boundary = match event {
            UiEvent::SessionUpdate(SessionUpdate::AgentMessageChunk(chunk)) => {
                let text = crate::event::content_block_text(&chunk.content);
                let previous = (self.lane == Some(SegmentLane::Thought))
                    .then(|| flush(self))
                    .flatten();
                self.lane = Some(SegmentLane::Message);
                self.segment.push_str(&text);
                self.final_message.push_str(&text);
                previous
            }
            UiEvent::SessionUpdate(SessionUpdate::AgentThoughtChunk(chunk)) => {
                let previous = (self.lane == Some(SegmentLane::Message))
                    .then(|| flush(self))
                    .flatten();
                self.lane = Some(SegmentLane::Thought);
                self.segment
                    .push_str(&crate::event::content_block_text(&chunk.content));
                previous
            }
            UiEvent::SessionUpdate(SessionUpdate::ToolCall(call)) => {
                let complete = matches!(
                    call.status,
                    ToolCallStatus::Completed | ToolCallStatus::Failed
                );
                self.tools
                    .insert(call.tool_call_id.to_string(), call.clone());
                complete.then(|| {
                    self.final_message.clear();
                    join_boundary(flush(self), render_tool_delta(call))
                })
            }
            UiEvent::SessionUpdate(SessionUpdate::ToolCallUpdate(update)) => {
                let id = update.tool_call_id.to_string();
                let completed = matches!(
                    update.fields.status,
                    Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
                );
                let rendered = {
                    let tool = self
                        .tools
                        .entry(id.clone())
                        .or_insert_with(|| ToolCall::new(id, "tool"));
                    tool.update(update.fields.clone());
                    completed.then(|| render_tool_delta(tool))
                };
                rendered.map(|rendered| {
                    self.final_message.clear();
                    join_boundary(flush(self), rendered)
                })
            }
            UiEvent::SessionUpdate(SessionUpdate::Plan(plan)) => {
                self.final_message.clear();
                Some(join_boundary(
                    flush(self),
                    bounded_item(format!("plan:\n{plan:?}")),
                ))
            }
            UiEvent::TerminalOutput(snapshot) if snapshot.exit_status.is_some() => {
                Some(join_boundary(
                    flush(self),
                    bounded_item(format!(
                        "terminal {} [{:?}]:\n{}",
                        snapshot.terminal_id, snapshot.exit_status, snapshot.output
                    )),
                ))
            }
            UiEvent::PromptDone { stop_reason, .. } => Some(join_boundary(
                flush(self),
                format!("turn finished: {stop_reason:?}"),
            )),
            UiEvent::PromptFailed { message } => Some(join_boundary(
                flush(self),
                format!("turn failed: {message}"),
            )),
            _ => None,
        };
        if let Some(boundary) = boundary.as_ref() {
            self.trajectory.push_str(boundary);
            self.trajectory.push('\n');
            self.trajectory = bounded(std::mem::take(&mut self.trajectory));
        }
        boundary
    }

    pub fn trajectory(&self) -> String {
        self.trajectory.clone()
    }

    pub fn final_message(&self) -> String {
        self.final_message.clone()
    }

    pub fn reset_attempt(&mut self) {
        self.final_message.clear();
        self.segment.clear();
        self.lane = None;
        self.tools.clear();
    }
}

fn render_tool_delta(tool: &agent_client_protocol::schema::v1::ToolCall) -> String {
    let mut text = format!("tool: {} [{:?}]", tool.title, tool.status);
    if let Some(input) = tool.raw_input.as_ref() {
        append_json_section(&mut text, "input", input);
    }
    if !tool.content.is_empty()
        && let Ok(content) = serde_json::to_value(&tool.content)
    {
        append_json_section(&mut text, "content", &content);
    }
    if let Some(output) = tool.raw_output.as_ref() {
        append_json_section(&mut text, "output", output);
    }
    bounded_item(text)
}

fn append_json_section(text: &mut String, label: &str, value: &serde_json::Value) {
    text.push('\n');
    text.push_str(label);
    text.push_str(":\n");
    text.push_str(&serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()));
}

fn bounded_item(mut text: String) -> String {
    if text.len() <= MAX_DELTA_ITEM_BYTES {
        return text;
    }
    let split = text.floor_char_boundary(MAX_DELTA_ITEM_BYTES);
    text.truncate(split);
    text.push_str("\n…[item truncated]");
    text
}

fn join_boundary(previous: Option<String>, current: String) -> String {
    previous.map_or(current.clone(), |previous| format!("{previous}\n{current}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    Thor,
    Eitri,
}

impl Target {
    fn label(self) -> &'static str {
        match self {
            Self::Thor => "Thor",
            Self::Eitri => "Eitri",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Verdict {
    NoIntervention,
    Intervention(String),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub id: u64,
    pub epoch: u64,
    pub target: Target,
    pub verdict: Verdict,
}

enum Request {
    Begin {
        epoch: u64,
        task: String,
    },
    TargetContext {
        epoch: u64,
        target: Target,
        text: String,
    },
    Review {
        id: u64,
        epoch: u64,
        target: Target,
        delta: String,
    },
    Shutdown,
}

#[derive(Debug, Clone)]
pub struct Handle {
    requests: mpsc::UnboundedSender<Request>,
    decisions: broadcast::Sender<Decision>,
    ids: Arc<AtomicU64>,
    epochs: Arc<AtomicU64>,
    abort: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    streaming_enabled: Arc<AtomicBool>,
}

impl Handle {
    pub fn start(
        role: ResolvedRole,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        ui_tx: mpsc::UnboundedSender<UiEvent>,
        streaming_enabled: bool,
    ) -> Self {
        let (requests, rx) = mpsc::unbounded_channel();
        let (decisions, _) = broadcast::channel(512);
        let (abort, abort_rx) = watch::channel(false);
        let (finished_tx, finished) = watch::channel(false);
        let handle = Self {
            requests,
            decisions: decisions.clone(),
            ids: Arc::new(AtomicU64::new(1)),
            epochs: Arc::new(AtomicU64::new(1)),
            abort,
            finished,
            streaming_enabled: Arc::new(AtomicBool::new(streaming_enabled)),
        };
        tokio::spawn(worker(
            role,
            cwd,
            additional_directories,
            ui_tx,
            rx,
            decisions,
            abort_rx,
            finished_tx,
        ));
        handle
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Decision> {
        self.decisions.subscribe()
    }

    pub fn begin_turn(&self, task: String) -> u64 {
        let _ = self.abort.send(false);
        let epoch = self.epochs.fetch_add(1, Ordering::Relaxed);
        let _ = self.requests.send(Request::Begin { epoch, task });
        epoch
    }

    pub fn begin_eitri(&self, epoch: u64, context: String) {
        let _ = self.requests.send(Request::TargetContext {
            epoch,
            target: Target::Eitri,
            text: context,
        });
    }

    pub fn current_epoch(&self) -> u64 {
        self.epochs.load(Ordering::Relaxed).saturating_sub(1)
    }

    pub fn cancel_turn(&self) {
        let _ = self.abort.send(true);
    }

    pub async fn observe(&self, epoch: u64, target: Target, delta: String) -> Option<u64> {
        if !self.streaming_enabled.load(Ordering::Acquire) {
            return None;
        }
        Some(self.submit(epoch, target, delta))
    }

    pub fn set_streaming_enabled(&self, enabled: bool) {
        self.streaming_enabled.store(enabled, Ordering::Release);
    }

    fn submit(&self, epoch: u64, target: Target, delta: String) -> u64 {
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let _ = self.requests.send(Request::Review {
            id,
            epoch,
            target,
            delta: bounded(delta),
        });
        id
    }

    pub async fn shutdown_and_wait(&self) {
        let _ = self.abort.send(true);
        let _ = self.requests.send(Request::Shutdown);
        let mut finished = self.finished.clone();
        if *finished.borrow() {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), finished.changed()).await;
    }
}

fn bounded(mut text: String) -> String {
    if text.len() > MAX_CONTEXT_BYTES {
        let split = text.len() - MAX_CONTEXT_BYTES;
        let split = text.ceil_char_boundary(split);
        text = format!("…[earlier context omitted]\n{}", &text[split..]);
    }
    text
}

#[allow(clippy::too_many_arguments)]
async fn worker(
    role: ResolvedRole,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    mut requests: mpsc::UnboundedReceiver<Request>,
    decisions: broadcast::Sender<Decision>,
    abort_rx: watch::Receiver<bool>,
    finished: watch::Sender<bool>,
) {
    let mut epoch = 0;
    let mut pending_context: HashMap<Target, String> = HashMap::new();
    let mut deferred = VecDeque::new();
    let mut session: Option<AgentHandle> = None;
    let advice = AdviceSlot::default();
    let server = match HttpServer::start(advice.clone(), decisions.clone()).await {
        Ok(server) => Some(server),
        Err(error) => {
            emit_warning(
                &ui_tx,
                &role,
                format!("Loki advisor tool could not start: {error:#}"),
            );
            None
        }
    };
    let mut primed = false;
    loop {
        let request = match deferred.pop_front() {
            Some(request) => request,
            None => match requests.recv().await {
                Some(request) => request,
                None => break,
            },
        };
        match request {
            Request::Begin {
                epoch: next,
                task: next_task,
            } => {
                epoch = next;
                pending_context.clear();
                pending_context.insert(
                    Target::Thor,
                    format!("New outer user request:\n{next_task}"),
                );
            }
            Request::TargetContext {
                epoch: request_epoch,
                target,
                text,
            } if request_epoch == epoch => {
                pending_context
                    .entry(target)
                    .and_modify(|pending| {
                        pending.push_str("\n\n");
                        pending.push_str(&text);
                    })
                    .or_insert(text);
            }
            Request::Review {
                id,
                epoch: request_epoch,
                target,
                mut delta,
            } if request_epoch == epoch => {
                let mut review_ids = vec![id];
                while let Ok(next) = requests.try_recv() {
                    match next {
                        Request::Review {
                            id: next_id,
                            epoch: next_epoch,
                            target: next_target,
                            delta: next_delta,
                        } if next_epoch == request_epoch && next_target == target => {
                            review_ids.push(next_id);
                            delta.push_str("\n\n--- next completed step ---\n\n");
                            delta.push_str(&next_delta);
                            delta = bounded(delta);
                        }
                        other => {
                            deferred.push_back(other);
                            break;
                        }
                    }
                }
                if *abort_rx.borrow() {
                    for id in review_ids {
                        let _ = decisions.send(Decision {
                            id,
                            epoch,
                            target,
                            verdict: Verdict::NoIntervention,
                        });
                    }
                    continue;
                }
                let Some(server) = server.as_ref() else {
                    for id in review_ids {
                        let _ = decisions.send(Decision {
                            id,
                            epoch,
                            target,
                            verdict: Verdict::Failed(
                                "Loki advisor tool is unavailable".to_string(),
                            ),
                        });
                    }
                    continue;
                };
                if session.is_none() {
                    match connect(
                        &role,
                        &cwd,
                        &additional_directories,
                        abort_rx.clone(),
                        server.advertised.clone(),
                    )
                    .await
                    {
                        Ok(agent) => {
                            session = Some(agent);
                            if let Err(error) = server.wait_until_tools_listed().await {
                                emit_warning(
                                    &ui_tx,
                                    &role,
                                    format!("Loki could not load advise: {error:#}"),
                                );
                                if let Some(old) = session.take() {
                                    old.dismiss().await;
                                }
                            }
                        }
                        Err(error) => {
                            if *abort_rx.borrow() {
                                for id in review_ids {
                                    let _ = decisions.send(Decision {
                                        id,
                                        epoch,
                                        target,
                                        verdict: Verdict::NoIntervention,
                                    });
                                }
                                continue;
                            }
                            let message = format!("Loki could not start: {error:#}");
                            emit_warning(&ui_tx, &role, message.clone());
                            for id in review_ids {
                                let _ = decisions.send(Decision {
                                    id,
                                    epoch,
                                    target,
                                    verdict: Verdict::Failed(message.clone()),
                                });
                            }
                            continue;
                        }
                    }
                }
                let Some(agent) = session.as_mut() else {
                    for id in review_ids {
                        let _ = decisions.send(Decision {
                            id,
                            epoch,
                            target,
                            verdict: Verdict::Failed(
                                "Loki could not load the advise tool".to_string(),
                            ),
                        });
                    }
                    continue;
                };
                let context = pending_context.remove(&target);
                let prompt = review_prompt(target, context.as_deref(), &delta, !primed);
                primed = true;
                advice.begin(id, epoch, target).await;
                let result = agent
                    .prompt(prompt, REVIEW_TIMEOUT, |event| match event {
                        TurnEvent::Permission {
                            prompt,
                            access_mode,
                        } => {
                            let decision = crate::ragnarok::permission_decision_for_access(
                                access_mode,
                                &prompt,
                            );
                            let _ = prompt.responder.send(decision);
                        }
                        TurnEvent::Message(_)
                        | TurnEvent::Thought(_)
                        | TurnEvent::Tool { .. }
                        | TurnEvent::Note(_) => {}
                    })
                    .await;
                let advised = advice.finish(id).await;
                if !advised {
                    let verdict = match result {
                        Ok(_) => Verdict::NoIntervention,
                        Err(_) if *abort_rx.borrow() => Verdict::NoIntervention,
                        Err(error) => {
                            let message = error.to_string();
                            emit_warning(
                                &ui_tx,
                                &role,
                                format!("Loki review failed open: {message}"),
                            );
                            primed = false;
                            Verdict::Failed(message)
                        }
                    };
                    for id in review_ids {
                        let _ = decisions.send(Decision {
                            id,
                            epoch,
                            target,
                            verdict: verdict.clone(),
                        });
                    }
                    if matches!(verdict, Verdict::Failed(_))
                        && let Some(old) = session.take()
                    {
                        old.dismiss().await;
                    }
                } else {
                    for id in review_ids.into_iter().skip(1) {
                        let _ = decisions.send(Decision {
                            id,
                            epoch,
                            target,
                            verdict: Verdict::NoIntervention,
                        });
                    }
                }
            }
            Request::TargetContext { .. } | Request::Review { .. } => {}
            Request::Shutdown => break,
        }
    }
    if let Some(agent) = session {
        agent.dismiss().await;
    }
    let _ = finished.send(true);
}

async fn connect(
    role: &ResolvedRole,
    cwd: &std::path::Path,
    additional_directories: &[PathBuf],
    abort: watch::Receiver<bool>,
    advise_server: McpServer,
) -> Result<AgentHandle> {
    let launch = Launch {
        program: role.launch.command.clone(),
        args: role.launch.args.clone(),
        env: role.launch.env.clone(),
    };
    AgentHandle::connect_with_role_config_and_mcp(
        &launch,
        cwd,
        additional_directories,
        abort,
        RuntimeAccessMode::ReadOnly,
        HashMap::new(),
        Some(RuntimeRoleConfig {
            label: "Loki".to_string(),
            model_value: role.model_value.clone(),
            force_high_reasoning: true,
        }),
        vec![advise_server],
    )
    .await
}

fn review_prompt(
    target: Target,
    context: Option<&str>,
    delta: &str,
    include_contract: bool,
) -> String {
    let mut prompt = String::new();
    if include_contract {
        prompt.push_str("You are Loki, Mjolnir's persistent pure advisor. You observe Thor and Eitri through incremental transcript updates. Your own messages are hidden. The advise MCP tool is your only channel back to the watched agent, and calling it forces Mjolnir to cancel that agent at the next safe step boundary and re-prompt it. Call advise at most once per update and only for a material correctness, safety, scope, or strategy concern. Your role is to make suggestions, not to issue review corrections. Phrase most advice as a question, such as \"Would it be better to...?\" or \"Have you thought about...?\" Stay silent for style, optional improvements, uncertainty, facts already visible to the target, and any update that is on track. Never implement changes yourself.\n\n");
    }
    if let Some(context) = context {
        prompt.push_str(context);
        prompt.push_str("\n\n");
    }
    prompt.push_str("### ");
    prompt.push_str(target.label());
    prompt.push_str(" session update\n\n");
    prompt.push_str(delta);
    prompt.push_str("\n\nConsider only this new update in light of your existing context. Use advise only if an advisory intervention is materially necessary; otherwise finish without commentary.");
    bounded(prompt)
}

fn identity(role: &ResolvedRole) -> LokiIdentity {
    LokiIdentity {
        role: "Loki".to_string(),
        connection_id: "loki".to_string(),
        source_id: Some(role.launch.source_id.clone()),
        model_name: Some(role.model.model.clone()),
        model_value: Some(role.model_value.clone()),
    }
}

fn emit_warning(ui_tx: &mpsc::UnboundedSender<UiEvent>, role: &ResolvedRole, message: String) {
    let _ = ui_tx.send(UiEvent::LokiActivity(LokiActivity::Warning {
        actor: identity(role),
        message,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{SessionUpdate, ToolCall, ToolCallStatus};

    fn test_handle(epoch: u64) -> (Handle, mpsc::UnboundedReceiver<Request>) {
        let (requests, request_rx) = mpsc::unbounded_channel();
        let (decisions, _) = broadcast::channel(8);
        let (abort, _) = watch::channel(false);
        let (_, finished) = watch::channel(false);
        (
            Handle {
                requests,
                decisions,
                ids: Arc::new(AtomicU64::new(1)),
                epochs: Arc::new(AtomicU64::new(epoch.saturating_add(1))),
                abort,
                finished,
                streaming_enabled: Arc::new(AtomicBool::new(true)),
            },
            request_rx,
        )
    }

    #[tokio::test]
    async fn advice_tool_slot_accepts_one_material_note_per_update() {
        let slot = AdviceSlot::default();
        slot.begin(4, 2, Target::Eitri).await;
        let decision = slot.accept("fix the race".to_string()).await.unwrap();
        assert_eq!(decision.id, 4);
        assert_eq!(decision.epoch, 2);
        assert_eq!(decision.target, Target::Eitri);
        assert!(
            matches!(decision.verdict, Verdict::Intervention(ref note) if note == "fix the race")
        );
        assert!(slot.accept("second note".to_string()).await.is_err());
        assert!(slot.finish(4).await);
    }

    #[test]
    fn intervention_waits_for_boundary_and_requests_one_cancellation() {
        let mut intervention = DeferredIntervention::default();

        intervention.push(7, "inspect the generated config".to_string());

        assert!(intervention.is_pending());
        assert!(!intervention.cancellation_was_requested());
        assert!(intervention.interrupt_at_boundary());
        assert!(intervention.cancellation_was_requested());
        assert!(!intervention.interrupt_at_boundary());
        assert_eq!(
            intervention.take().as_deref(),
            Some("inspect the generated config")
        );
        assert!(!intervention.is_pending());
        assert!(!intervention.cancellation_was_requested());
    }

    #[test]
    fn completed_target_can_be_reprompted_without_cancellation() {
        let mut intervention = DeferredIntervention::default();
        intervention.push(3, "fix the final answer".to_string());

        assert_eq!(intervention.take().as_deref(), Some("fix the final answer"));
        assert!(!intervention.cancellation_was_requested());
    }

    #[test]
    fn queued_critiques_are_ordered_and_clear_resets_boundary_state() {
        let mut intervention = DeferredIntervention::default();
        intervention.push(20, "second observed review".to_string());
        intervention.push(10, "first observed review".to_string());
        assert!(intervention.interrupt_at_boundary());

        assert_eq!(
            intervention.take().as_deref(),
            Some("first observed review\n\nAdditional Loki critique: second observed review")
        );

        intervention.push(30, "stale review".to_string());
        assert!(intervention.interrupt_at_boundary());
        intervention.clear();
        assert!(!intervention.is_pending());
        assert!(!intervention.cancellation_was_requested());
        assert!(!intervention.interrupt_at_boundary());
    }

    #[test]
    fn tool_reviews_only_run_at_completed_or_failed_boundaries() {
        let mut tracker = BoundaryTracker::default();
        let tool = |status| {
            UiEvent::SessionUpdate(SessionUpdate::ToolCall(
                ToolCall::new("tool-1", "build").status(status),
            ))
        };

        assert!(tracker.observe(&tool(ToolCallStatus::Pending)).is_none());
        assert!(tracker.observe(&tool(ToolCallStatus::InProgress)).is_none());
        assert!(tracker.observe(&tool(ToolCallStatus::Completed)).is_some());
        assert!(tracker.observe(&tool(ToolCallStatus::Failed)).is_some());
    }

    #[test]
    fn completed_tool_delta_contains_bounded_input_and_output() {
        let mut tracker = BoundaryTracker::default();
        let event = UiEvent::SessionUpdate(SessionUpdate::ToolCall(
            ToolCall::new("tool-1", "run tests")
                .raw_input(serde_json::json!({"command": "cargo test"}))
                .raw_output(serde_json::json!({"exit": 1, "stderr": "boom"}))
                .status(ToolCallStatus::Failed),
        ));
        let delta = tracker.observe(&event).expect("completed tool boundary");
        assert!(delta.contains("tool: run tests [Failed]"));
        assert!(delta.contains("cargo test"));
        assert!(delta.contains("boom"));
    }

    #[tokio::test]
    async fn every_completed_step_is_submitted_without_a_workspace_change_gate() {
        let (handle, mut requests) = test_handle(7);
        assert!(
            handle
                .observe(7, Target::Thor, "planning only".to_string())
                .await
                .is_some()
        );
        let Request::Review { target, delta, .. } = requests.try_recv().expect("review request")
        else {
            panic!("expected review request");
        };
        assert_eq!(target, Target::Thor);
        assert_eq!(delta, "planning only");
    }

    #[test]
    fn advisor_prompt_primes_once_and_keeps_updates_hidden_and_incremental() {
        let first = review_prompt(
            Target::Thor,
            Some("New outer user request:\nfix it"),
            "tool: inspect [Completed]",
            true,
        );
        assert!(first.contains("persistent pure advisor"));
        assert!(first.contains("suggestions, not to issue review corrections"));
        assert!(first.contains("Phrase most advice as a question"));
        assert!(first.contains("New outer user request"));
        assert!(first.contains("### Thor session update"));
        let later = review_prompt(Target::Eitri, None, "thinking:\nchecking", false);
        assert!(!later.contains("persistent pure advisor"));
        assert!(later.contains("### Eitri session update"));
        assert!(!later.contains("fix it"));
    }
}

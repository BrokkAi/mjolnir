//! One-shot nested ACP agent orchestration exposed to the primary agent as MCP.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agent_client_protocol::schema::v1::{SessionUpdate, StopReason};
use anyhow::{Context, Result, anyhow, bail};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, Content, Implementation, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc, watch};

use crate::acp::{self, AcpRuntimeConfig, RuntimeAccessMode};
use crate::event::{
    CodeAgentEvent, CodeAgentOutcome, InternalMessage, InternalMessageKind, UiCommand, UiEvent,
    content_block_text,
};
use crate::loki;

pub const LABEL: &str = "Eitri";
pub const MCP_SERVER_NAME: &str = "mj-code-agent";
pub const PRIMARY_SESSION_DIRECTIVE: &str = "<mj-code-agent-policy>\nYou are Thor, the primary coordinator and owner of the user's outcome. You are responsible for understanding the request, doing necessary research and context gathering, forming the plan, coordinating implementation, reviewing and verifying the result, and delivering the final answer. You are not a thin handoff between the user and Eitri. Eitri is the implementation agent exposed as the code_agent MCP tool. This policy applies to every subsequent user request in this ACP session. Delegate substantial implementation chunks to Eitri after you have investigated and planned enough to give useful direction. You may personally make small, local code changes when describing and delegating them would take more effort than simply doing them; use judgment rather than delegating mechanically. Eitri starts a brand-new ACP process and session for every code_agent call. Eitri has no conversation context and no memory of the user's request, your research, or any previous Eitri call—even the immediately preceding call. Every invocation must therefore contain complete standalone instructions with the task, relevant findings, your plan, current workspace state, and acceptance criteria. After Eitri returns, independently review its result, inspect or verify the work as needed, and delegate a substantial corrective follow-up if implementation changes remain. If a request requires no code changes, handle it yourself. Do not call any tool now. Acknowledge this policy with exactly MJ_CODE_AGENT_POLICY_READY.\n</mj-code-agent-policy>";

const FRESH_CONTEXT_PREAMBLE: &str = "You are Eitri, the implementation agent. This is a fresh ACP process and session. You have no memory of the user conversation or of any earlier Eitri call, including an immediately preceding call. Treat the standalone instructions below and the current workspace as your only task context.\n\n";

#[derive(Debug, Clone)]
pub struct Config {
    pub display_label: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub agent_stderr: Option<PathBuf>,
    pub role_config: Option<acp::RuntimeRoleConfig>,
    pub loki: Option<loki::Handle>,
    pub delegation_observer: Option<Arc<AtomicBool>>,
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
            delegation_observer: None,
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
            delegation_observer: None,
        }
    }

    pub fn with_delegation_observer(mut self, observer: Arc<AtomicBool>) -> Self {
        self.delegation_observer = Some(observer);
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
        description = "IMPLEMENTATION DELEGATE (EITRI). Thor owns research, planning, coordination, review, verification, and the final response. Delegate substantial implementation chunks to Eitri, but make small local changes directly when describing and delegating them would take more effort than doing them. Every call starts a fresh ACP process/session with zero conversation or prior-call memory. Pass complete standalone instructions with the task, plan, relevant findings, current workspace state, and acceptance criteria. Review Eitri's result independently and call it again for substantial corrections."
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
                "a code agent run is already active",
            )]));
        }

        let result = run_boxed(
            self.config.clone(),
            self.context.clone(),
            args.instructions,
            self.ui_tx.clone(),
            self.controller.clone(),
        )
        .await;
        Ok(match result {
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
                "DELEGATION POLICY: Thor owns research, planning, coordination, review, verification, and the final answer. Delegate substantial implementation chunks to Eitri; Thor may directly make small local changes when delegation would cost more effort. Every code_agent call is a fresh ACP process/session with zero conversation or prior-call memory, so every invocation needs complete standalone instructions.",
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

/// Serve the `code_agent` tool on an in-process loopback endpoint, advertised
/// to the primary ACP agent as a stdio `mj mcp-proxy` server.
pub async fn start_server(
    config: Config,
    context: RunContext,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    controller: Controller,
) -> Result<crate::loopback_mcp::LoopbackServer> {
    let (tools_listed_tx, tools_listed) = watch::channel(false);
    let handler = McpHandler::new(config, context, ui_tx, controller, tools_listed_tx);
    crate::loopback_mcp::LoopbackServer::start(MCP_SERVER_NAME, "code-agent", handler, tools_listed)
        .await
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

/// Coordinates the one allowed nested run and routes primary UI cancellation
/// without giving the nested runtime ownership of the primary command stream.
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
            bail!("code agent completed without a final message");
        }
        Ok(self.last.clone())
    }
}

pub fn run_boxed(
    config: Config,
    context: RunContext,
    instructions: String,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    controller: Controller,
) -> futures::future::BoxFuture<'static, Result<String>> {
    Box::pin(run(config, context, instructions, ui_tx, controller))
}

async fn run(
    config: Config,
    context: RunContext,
    instructions: String,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
    controller: Controller,
) -> Result<String> {
    if let Some(observer) = config.delegation_observer.as_ref() {
        observer.store(true, Ordering::Release);
    }
    let standalone_prompt = format!("{FRESH_CONTEXT_PREAMBLE}{instructions}");
    let display_label = config.display_label.clone();
    let _ = ui_tx.send(UiEvent::InternalMessage(InternalMessage {
        source: "Thor".to_string(),
        target: LABEL.to_string(),
        kind: InternalMessageKind::Delegation,
        text: instructions.clone(),
    }));
    let _ = ui_tx.send(UiEvent::CodeAgent(CodeAgentEvent::Started {
        label: display_label,
    }));

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
        access_mode: context.access_mode,
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
        reviewer.begin_eitri(epoch, instructions.clone());
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
                    Ok(Ok(())) => Err(anyhow!("code agent runtime closed before completing")),
                    Ok(Err(error)) => Err(error).context("code agent runtime"),
                    Err(error) => Err(anyhow!("code agent task failed: {error}")),
                };
            }
            event = nested_event_rx.recv() => {
                let Some(event) = event else {
                    break Err(anyhow!("code agent event stream closed before completing"));
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
                            break Err(anyhow!("send instructions to code agent"));
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
                                break Err(anyhow!("code agent cancelled"));
                            }
                            if let Some(critique) = intervention.take() {
                                collector = AgentMessageCollector::new();
                                tracker.reset_attempt();
                                pending_reviews.clear();
                                completed = None;
                                let continuation = continuation_prompt(&instructions, &critique, &tracker.trajectory());
                                emit_continuation(&ui_tx, &critique);
                                if nested_cmd_tx.send(UiCommand::SendPrompt { text: continuation, images: Vec::new() }).is_err() {
                                    break Err(anyhow!("re-prompt Eitri after Loki intervention"));
                                }
                                continue;
                            }
                            break Err(anyhow!("code agent cancelled"));
                        }
                        if let Some(critique) = intervention.take() {
                            collector = AgentMessageCollector::new();
                            tracker.reset_attempt();
                            pending_reviews.clear();
                            completed = None;
                            let continuation = continuation_prompt(&instructions, &critique, &tracker.trajectory());
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
                            let continuation = continuation_prompt(&instructions, &critique, &tracker.trajectory());
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
                    | UiEvent::ActorActivity(_)
                    | UiEvent::InternalMessage(_) => {}
                    UiEvent::CodeAgent(_) => {
                        break Err(anyhow!("nested code agent attempted recursive delegation"));
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
                            let continuation = continuation_prompt(&instructions, &critique, &tracker.trajectory());
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
    }
    controller.finish().await;

    let outcome = match &result {
        Ok(_) => CodeAgentOutcome::Completed,
        Err(error) if error.to_string().contains("cancelled") => CodeAgentOutcome::Cancelled,
        Err(error) => CodeAgentOutcome::Failed(error.to_string()),
    };
    let _ = ui_tx.send(UiEvent::CodeAgent(CodeAgentEvent::Finished { outcome }));
    result
}

fn continuation_prompt(_task: &str, critique: &str, _trajectory: &str) -> String {
    format!(
        "<advisory guidance=\"weigh, don't blindly obey\">\n{critique}\n</advisory>\n\nContinue the interrupted implementation turn. Address the material advice, then finish the existing task."
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
    }

    #[test]
    fn tool_is_model_visible_and_directs_coding_delegation() {
        let tools = test_handler().tool_router.list_all();
        assert_eq!(tools.len(), 1);
        let tool = serde_json::to_value(&tools[0]).expect("serialize tool");
        assert_eq!(tool["name"], "code_agent");
        let description = tool["description"].as_str().expect("description");
        assert!(description.contains("IMPLEMENTATION DELEGATE"));
        assert!(description.contains("Thor owns research, planning"));
        assert!(description.contains("substantial implementation chunks"));
        assert!(description.contains("small local changes directly"));
        assert!(description.contains("fresh ACP process/session"));
        assert!(description.contains("zero conversation or prior-call memory"));
        assert!(description.contains("Review Eitri's result independently"));
        assert_eq!(
            tool["inputSchema"]["required"],
            serde_json::json!(["instructions"])
        );
    }

    #[test]
    fn primary_directive_makes_thor_the_coordinator_not_a_thin_router() {
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("You are not a thin handoff"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("research and context gathering"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("forming the plan"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("independently review its result"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("Delegate substantial implementation chunks"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("small, local code changes"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("would take more effort"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("brand-new ACP process and session"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("immediately preceding call"));
        assert!(PRIMARY_SESSION_DIRECTIVE.contains("complete standalone instructions"));
        assert!(!PRIMARY_SESSION_DIRECTIVE.contains("before using any other tool"));
    }

    #[test]
    fn eitri_preamble_explicitly_declares_fresh_context() {
        assert!(FRESH_CONTEXT_PREAMBLE.contains("fresh ACP process and session"));
        assert!(FRESH_CONTEXT_PREAMBLE.contains("no memory of the user conversation"));
        assert!(FRESH_CONTEXT_PREAMBLE.contains("immediately preceding call"));
        assert!(FRESH_CONTEXT_PREAMBLE.contains("current workspace"));
    }
}

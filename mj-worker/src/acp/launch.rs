use super::*;

/// One observed identity shared by credential installation and prompt dispatch.
/// Sampling supplemental recovery evidence never performs filesystem I/O.
#[derive(Debug, Clone, Default)]
pub struct ClaudeLoginIdentity(Arc<ClaudeLoginState>);

#[derive(Debug, Default)]
struct ClaudeLoginState {
    #[cfg_attr(not(unix), allow(dead_code))]
    installation: tokio::sync::Mutex<()>,
    observed: Mutex<(u64, Option<String>)>,
}

#[cfg_attr(not(unix), allow(dead_code))]
impl ClaudeLoginIdentity {
    pub(crate) fn snapshot(&self) -> (u64, Option<String>) {
        self.0
            .observed
            .lock()
            .expect("Claude login identity lock poisoned")
            .clone()
    }

    pub(crate) fn observe(&self, expected: u64, fingerprint: Option<String>) {
        let mut observed = self
            .0
            .observed
            .lock()
            .expect("Claude login identity lock poisoned");
        if observed.0 == expected {
            observed.1 = fingerprint;
        }
    }

    pub(crate) fn installed(&self, fingerprint: Option<String>) {
        let mut observed = self
            .0
            .observed
            .lock()
            .expect("Claude login identity lock poisoned");
        observed.0 = observed
            .0
            .checked_add(1)
            .expect("Claude login revision exhausted");
        observed.1 = fingerprint;
    }

    pub(crate) async fn installation(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.0.installation.lock().await
    }
}

#[cfg(unix)]
pub(crate) async fn read_claude_login_fingerprint(path: PathBuf) -> Result<Option<String>> {
    let (completed, result) = oneshot::channel();
    // A cancelled file read cannot be interrupted. Keep this read-only thread
    // outside Tokio's pool, whose shutdown waits for even cancelled blocking
    // jobs. Dropping its handle lets the worker exit if the filesystem stalls.
    let _reader = std::thread::Builder::new()
        .name("claude-login-read".into())
        .spawn(move || {
            let read = std::panic::catch_unwind(|| {
                let (_, bytes) =
                    mj_core::credentials::read_credential_file(HarnessKind::Claude, &path)?;
                Ok(mj_core::credentials::claude_access_token_fingerprint(
                    &bytes,
                ))
            });
            let read = read.unwrap_or_else(|_| Err(anyhow!("Claude login reader panicked")));
            if let Err(Err(error)) = completed.send(read) {
                tracing::warn!(%error, "Claude login read failed after its caller stopped");
            }
        })
        .context("start Claude login reader")?;
    result.await.context("read Claude login identity")?
}

/// The worker's sub-agent socket and the `mj-agents` role it backs: a parent
/// delegates through it, a child only hands its report back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentMcpSocket {
    pub path: PathBuf,
    pub role: mj_core::subagent::SubagentMcpRole,
    pub agent_mailboxes_enabled: bool,
    /// Whether a parent's spawn tool offers the subtle task type.
    pub subtle_subagents: bool,
    /// Whether this launch offers the `mj-review` server beside `mj-agents`.
    pub review_tools: bool,
    /// False for legacy shared Codex homes, which must retain ACP delivery.
    pub profile_registration: bool,
}

/// One configured MCP server exposed to the reviewer harness.
#[derive(Debug, Clone)]
pub struct ReviewerMcpServer {
    server: mj_core::worker_launch::ReviewMcpServer,
}

impl ReviewerMcpServer {
    pub fn new(server: mj_core::worker_launch::ReviewMcpServer) -> Self {
        Self { server }
    }
}

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub login_identity: ClaudeLoginIdentity,
    /// Live availability shared with the worker control connection.
    pub operator_away: OperatorAway,
    pub clear_context_request: Option<ContextReset>,
    /// Settings to restore when a failed clear reloads the old conversation.
    pub context_restore: Option<ContextReset>,
    pub goal_recovery: Arc<Mutex<mj_core::goal::GoalRecoveryContext>>,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub project_memory: Option<ProjectMemoryLaunchConfig>,

    /// Extra stdio MCP servers this session gets beyond project memory.
    /// Reviewers can receive configured servers here; the primary gets none.
    pub extra_mcp_servers: Vec<ReviewerMcpServer>,
    /// Delegation policy independently controls native tool suppression.
    pub subagent_policy: mj_core::subagent::SubagentPolicy,
    /// This session belongs to another Mjolnir session, independently of its MCP role.
    pub managed_subagent: bool,
    /// Private Mjolnir socket: parent delegation or child handback.
    pub subagent_mcp_socket: Option<SubagentMcpSocket>,
    /// The supervisor spec the bridge command reads, when this launch has
    /// one. It is rewritten from `accepted_config` before every bridge start,
    /// so a harness that can only take a selector before it opens a session
    /// gets the value this session accepted at *this* launch rather than the
    /// one it held when the worker started.
    pub bridge_spec_path: Option<PathBuf>,
    pub resume_session: Option<String>,
    /// Whether Mjolnir's durable state shows the native session named by
    /// `resume_session` may already hold conversation history. Codex writes a
    /// thread's rollout, and Claude Code a session's transcript, only at the
    /// first user message, so a native session that was created and never
    /// prompted is missing on disk; when this is false, a resume that fails
    /// because the harness has no such session is answered with a fresh
    /// session in the same Mjolnir session instead of a dead worker.
    pub native_session_may_have_history: bool,
    /// Accepted selectors for this logical session, shared across native
    /// bridge replacements. Workers seed this from their durable relay.
    pub accepted_config: Arc<Mutex<AcceptedSessionConfig>>,
    /// The model this session was created for, from the launch config. See
    /// [`LaunchSpec::startup_model`].
    pub initial_model: Option<String>,
    pub harness: HarnessKind,
    /// For OpenCode, the staged logins this bridge's launch imported; the
    /// idle session reloads once they change. Set when the bridge starts.
    pub imported_logins: Option<super::ImportedLogins>,
    pub execution_policy: ExecutionPolicy,
    /// The Claude profile's permission mode under configured approvals, from
    /// the launch config. Read only through `session_enforcement`.
    pub claude_permission_mode: mj_core::config::ClaudePermissionMode,
    /// Optional central Brokk Observe guardrail client. The permission
    /// handler consults it before any automatic approval.
    pub observe_policy: Option<crate::observe::PolicyClient>,
    pub acp_activity: AcpActivityClock,
    /// When the step the agent is on began. Marked from the same handlers as
    /// `acp_activity`, but only where a new step actually starts.
    pub step_clock: StepClock,
    /// The tool calls the agent has open, shared with the durable relay that
    /// records them. The turn stall watchdog reads this: a turn blocked in a
    /// long tool call is working, however silent the protocol is (#1020).
    pub tools_in_flight: mj_core::activity::ToolsInFlight,
    /// How long a running turn may go without a sign of life. `None` reads the
    /// process environment, which is what every launch does; a test sets it
    /// directly so it does not have to reach for a global.
    pub turn_context: mj_transcript::turn_context::TurnContext,
    /// Retained for source compatibility. Active ACP turns never consult Jev;
    /// completed-turn classification belongs to the durable relay coordinator.
    pub verdict: Option<VerdictSource>,
    pub stall_policy: Option<mj_core::activity::StallPolicy>,
}

impl LaunchSpec {
    /// The model the harness opens its session on: the one this session
    /// accepted, else the one it was created for.
    ///
    /// A harness that can only take its model before the session opens
    /// (Codex through `CODEX_CONFIG`, Claude through `session/new`) otherwise
    /// starts a new session on the profile default and is switched before
    /// its first turn. Codex prewarms its Responses connection with the
    /// default, and a sub-agent's first request for another model over that
    /// connection was refused (issue 1217). The accepted model wins as soon
    /// as there is one, so a model chosen later is never replaced by the one
    /// the session was created with.
    pub fn startup_model(&self) -> Option<String> {
        self.accepted_config
            .lock()
            .expect("accepted session configuration lock poisoned")
            .model
            .clone()
            .or_else(|| self.initial_model.clone())
    }
}

pub(super) fn project_history_mcp(spec: &LaunchSpec) -> Vec<McpServer> {
    if spec
        .project_memory
        .as_ref()
        .is_some_and(|memory| memory.mcp_delivery == ProjectMemoryMcpDelivery::HarnessProfile)
    {
        return Vec::new();
    }
    let Some(memory) = &spec.project_memory else {
        return Vec::new();
    };
    if spec.harness == HarnessKind::Claude && memory.history_socket.is_none() {
        return Vec::new();
    }
    let mut args = vec!["worker".into(), "memory-mcp".into()];
    if let Some(socket) = &memory.history_socket {
        args.extend([
            "--history-socket".into(),
            socket.to_string_lossy().into_owned(),
        ]);
    }
    vec![McpServer::Stdio(approve_owned_mcp(
        spec,
        McpServerStdio::new("mj-memory", spec.command.clone()).args(args),
    ))]
}

pub(super) fn session_request_meta(
    spec: &LaunchSpec,
    include_history_tools: bool,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if spec.harness == HarnessKind::Codex {
        let asking = spec
            .goal_recovery
            .lock()
            .expect("goal lock poisoned")
            .asking();
        let mut meta = serde_json::Map::from_iter([(
            "jetbrains".into(),
            serde_json::json!({"air":{"goal":{"resumePolicy": if asking {"pause"} else {"preserve"}}}}),
        )]);
        // The launch policy owns suppression independently of MCP exposure:
        // None and children suppress native delegation without a parent socket.
        if spec.subagent_policy.suppresses_native() {
            meta.insert(
                "codex".into(),
                serde_json::json!({"options":{"disallowedTools":["spawn_agent"]}}),
            );
        }
        return Some(meta);
    }
    if spec.harness != HarnessKind::Claude {
        return None;
    }
    // Raw results cover cycles without an AIR task. Foreground prompts end
    // at PromptResponse, and announced autonomous turns at their task end.
    let mut claude_code = serde_json::Map::from_iter([(
        "emitRawSDKMessages".to_owned(),
        serde_json::json!([
            {"type": "result"},
        ]),
    )]);
    let mut options = serde_json::Map::from_iter([(
        "perTaskStopAffordance".to_owned(),
        serde_json::Value::Bool(true),
    )]);
    // Claude builds its resume catalogue before ACP selector restoration. With
    // setup-token auth, only an explicit startup pin retains the 1M model row.
    if let Some(model) = spec.startup_model() {
        options.insert("model".to_owned(), serde_json::Value::String(model));
    }
    if let Some(enabled) = super::session_config::session_enforcement(spec)
        .and_then(mj_core::config::ExecutionEnforcement::session_sandbox)
    {
        options.insert(
            "sandbox".to_owned(),
            serde_json::json!({ "enabled": enabled }),
        );
    }
    // Hide native discovery, messaging/resumption and workflow orchestration too.
    // TaskStop stays because it also manages background shell commands.
    if spec.subagent_policy.suppresses_native() {
        options.insert(
            "disallowedTools".to_owned(),
            serde_json::json!(["Agent", "Task", "ListAgents", "SendMessage", "Workflow"]),
        );
    }
    if spec.execution_policy == ExecutionPolicy::ConfiguredApprovals {
        let mut allowed = vec!["Bash(gh:*)".to_owned()];
        if include_history_tools {
            for server in project_history_mcp(spec) {
                if let McpServer::Stdio(server) = server {
                    allowed.push(format!("mcp__{}__*", server.name));
                }
            }
        }
        if let Some(socket) = &spec.subagent_mcp_socket {
            allowed.extend(
                socket
                    .role
                    .tool_names()
                    .iter()
                    .map(|tool| format!("mcp__{}__{tool}", mj_core::subagent::SUBAGENT_MCP_SERVER)),
            );
        }
        if spec
            .subagent_mcp_socket
            .as_ref()
            .is_some_and(|socket| socket.review_tools)
        {
            allowed.extend(
                mj_core::subagent::REVIEW_MCP_TOOLS
                    .iter()
                    .map(|tool| format!("mcp__{}__{tool}", mj_core::subagent::REVIEW_MCP_SERVER)),
            );
        }
        options.insert("allowedTools".to_owned(), serde_json::json!(allowed));
    }
    claude_code.insert("options".to_owned(), serde_json::Value::Object(options));
    Some(serde_json::Map::from_iter([(
        "claudeCode".to_owned(),
        serde_json::Value::Object(claude_code),
    )]))
}

/// The reviewer's configured MCP servers, for harnesses that accept a server
/// over ACP. Claude and Kimi read their staged profile instead, which the
/// controller writes while staging the reviewer.
pub(super) fn extra_mcp(spec: &LaunchSpec) -> Vec<McpServer> {
    let mut servers = if mj_core::worker_launch::ReviewMcpDelivery::for_harness(spec.harness)
        == mj_core::worker_launch::ReviewMcpDelivery::Acp
    {
        spec.extra_mcp_servers
            .iter()
            .map(|entry| {
                let server = &entry.server;
                let registration = McpServerStdio::new(server.name.clone(), server.command.clone())
                    .args(server.args.clone());
                McpServer::Stdio(registration)
            })
            .collect()
    } else {
        Vec::new()
    };
    servers.extend(subagent_acp_mcp(spec));
    servers.extend(review_acp_mcp(spec));
    servers
}

/// The `mj-agents` server, when this launch delivers it over ACP. The worker
/// decides delivery after checking profile ownership. Legacy shared Codex
/// homes keep ACP delivery until the controller restages them.
fn subagent_acp_mcp(spec: &LaunchSpec) -> Option<McpServer> {
    if spec.harness == HarnessKind::Claude {
        return None;
    }
    let socket = spec.subagent_mcp_socket.as_ref()?;
    if socket.profile_registration {
        return None;
    }
    let worker = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("hel"));
    let mut args = vec![
        "worker".into(),
        "subagent-mcp".into(),
        "--socket".into(),
        socket.path.to_string_lossy().into_owned(),
        // The server bounds a `wait` by what this harness's own MCP
        // client will hold open.
        "--harness".into(),
        spec.harness.id().to_owned(),
        "--role".into(),
        socket.role.id().to_owned(),
        "--agent-mailboxes-enabled".into(),
        socket.agent_mailboxes_enabled.to_string(),
    ];
    if !socket.subtle_subagents {
        // Only the non-default value is passed, for older workers.
        args.extend(["--subtle-subagents".into(), "false".into()]);
    }
    Some(McpServer::Stdio(approve_owned_mcp(
        spec,
        McpServerStdio::new(mj_core::subagent::SUBAGENT_MCP_SERVER, worker).args(args),
    )))
}

/// The `mj-review` server, when this launch offers it and delivers it over
/// ACP, on the same terms as `mj-agents`.
fn review_acp_mcp(spec: &LaunchSpec) -> Option<McpServer> {
    if spec.harness == HarnessKind::Claude {
        return None;
    }
    let socket = spec.subagent_mcp_socket.as_ref()?;
    if socket.profile_registration || !socket.review_tools {
        return None;
    }
    let worker = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("hel"));
    let args = socket
        .role
        .review_server_args(&socket.path, spec.harness.id());
    Some(McpServer::Stdio(approve_owned_mcp(
        spec,
        McpServerStdio::new(mj_core::subagent::REVIEW_MCP_SERVER, worker).args(args),
    )))
}

/// The Mjolnir-owned MCP servers this launch registers over ACP, each with the
/// tools it lists: project history, delegation and review. Reviewer servers a
/// person configured are not Mjolnir's, so they are never in this list.
pub(super) fn owned_acp_mcp_tools(spec: &LaunchSpec) -> Vec<(String, Vec<String>)> {
    let names = |tools: &[&str]| tools.iter().map(|tool| (*tool).to_owned()).collect();
    let mut owned = Vec::new();
    for server in project_history_mcp(spec) {
        if let McpServer::Stdio(server) = server {
            // The history server lists its tools only with a history socket.
            let tools = spec
                .project_memory
                .as_ref()
                .is_some_and(|memory| memory.history_socket.is_some())
                .then(crate::memory_mcp::tool_names)
                .unwrap_or_default();
            owned.push((server.name, tools));
        }
    }
    if let (Some(McpServer::Stdio(server)), Some(socket)) =
        (subagent_acp_mcp(spec), &spec.subagent_mcp_socket)
    {
        owned.push((server.name, names(socket.role.tool_names())));
    }
    if let Some(McpServer::Stdio(server)) = review_acp_mcp(spec) {
        owned.push((server.name, names(mj_core::subagent::REVIEW_MCP_TOOLS)));
    }
    owned
}

/// Only owned registrations opt in; yolo and other harnesses stay unchanged.
fn approve_owned_mcp(spec: &LaunchSpec, server: McpServerStdio) -> McpServerStdio {
    if spec.harness == HarnessKind::Codex
        && spec.execution_policy == ExecutionPolicy::ConfiguredApprovals
    {
        server.meta(serde_json::Map::from_iter([(
            "codex".into(),
            serde_json::json!({"defaultToolsApprovalMode": "approve"}),
        )]))
    } else {
        server
    }
}

fn session_mcp(spec: &LaunchSpec, include_history_tools: bool) -> Vec<McpServer> {
    let mut servers = extra_mcp(spec);
    if include_history_tools {
        servers.extend(project_history_mcp(spec));
    }
    servers
}

/// Environment variables that name Mjolnir's build-tool cache directories. The
/// worker adds these to the session's additional directories, and the notice
/// for directories a harness cannot take uses them to say what was dropped.
pub(crate) const TOOL_CACHE_DIRECTORY_VARIABLES: [&str; 3] =
    ["MJ_TOOL_CACHE_DIR", "MJ_NX_HOME_DIR", "MJ_TOOL_CACHE_HOME"];

/// The additional directories one ACP connection can take, and what could not
/// be passed.
///
/// An agent that accepts `additionalDirectories` says so in
/// `sessionCapabilities`. One that does not rejects a non-empty list and
/// fails session creation (Grok Build: "a session works only in its cwd",
/// #1433), so the list is withheld from it and the session starts with its
/// working directory alone.
pub(super) struct SessionDirectories {
    pub accepted: Vec<PathBuf>,
    /// Set when directories were withheld: what the person should know.
    pub notice: Option<String>,
}

impl SessionDirectories {
    pub(super) fn negotiate(
        spec: &LaunchSpec,
        capabilities: &agent_client_protocol::v1::schema::SessionCapabilities,
    ) -> Self {
        if capabilities.additional_directories.is_some() || spec.additional_directories.is_empty() {
            return Self {
                accepted: spec.additional_directories.clone(),
                notice: None,
            };
        }
        let tool_cache = TOOL_CACHE_DIRECTORY_VARIABLES
            .iter()
            .filter_map(|name| spec.environment.get(*name).map(PathBuf::from))
            .collect::<Vec<_>>();
        let described = spec
            .additional_directories
            .iter()
            .map(|directory| {
                let kind = if spec
                    .project_memory
                    .as_ref()
                    .is_some_and(|memory| &memory.root == directory)
                {
                    "project memory"
                } else if tool_cache.contains(directory) {
                    "tool cache"
                } else {
                    "workspace directory"
                };
                format!("{kind} {}", directory.display())
            })
            .collect::<Vec<_>>()
            .join("; ");
        Self {
            accepted: Vec::new(),
            notice: Some(format!(
                "{} does not accept additional directories over ACP and limits a session to its working directory, so this session starts without them: {described}. The agent may not be able to read or write these directories.",
                spec.harness.display_name()
            )),
        }
    }
}

pub(super) fn new_session_request(
    spec: &LaunchSpec,
    directories: &[PathBuf],
    include_history_tools: bool,
) -> NewSessionRequest {
    let request = NewSessionRequest::new(spec.cwd.clone())
        .additional_directories(directories.to_vec())
        .meta(session_request_meta(spec, include_history_tools));
    request.mcp_servers(session_mcp(spec, include_history_tools))
}

pub(super) fn load_session_request(
    spec: &LaunchSpec,
    directories: &[PathBuf],
    session_id: SessionId,
) -> LoadSessionRequest {
    LoadSessionRequest::new(session_id, spec.cwd.clone())
        .additional_directories(directories.to_vec())
        // The servers Mjolnir owns travel on every launch, because a bridge
        // that opens the session again is a new harness process: Codex builds
        // the resumed thread's MCP set from this request and recovers nothing
        // it was not given (#1085). Replay filtering belongs to the notification
        // handler; omitting history here removes its tools after restart.
        .mcp_servers(session_mcp(spec, true))
        .meta(session_request_meta(spec, true))
}

pub(super) fn resume_session_request(
    spec: &LaunchSpec,
    directories: &[PathBuf],
    session_id: SessionId,
) -> ResumeSessionRequest {
    ResumeSessionRequest::new(session_id, spec.cwd.clone())
        .additional_directories(directories.to_vec())
        .mcp_servers(session_mcp(spec, true))
        .meta(session_request_meta(spec, true))
}

/// Settings captured from the old native conversation before retiring it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextReset {
    pub request_id: String,
    pub selectors: Vec<(String, String)>,
    pub mode: Option<String>,
}

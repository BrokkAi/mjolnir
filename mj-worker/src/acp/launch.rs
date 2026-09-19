use super::*;

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub goal_recovery: Arc<Mutex<mj_core::goal::GoalRecoveryContext>>,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub project_memory: Option<ProjectMemoryLaunchConfig>,
    /// Extra stdio MCP servers this session gets, beyond project memory. A
    /// turn review's reviewing agents get Bifrost this way; the primary
    /// session gets none.
    pub extra_mcp_servers: Vec<mj_core::worker_launch::ReviewMcpServer>,
    /// Private Mjolnir delegation socket for supported parent sessions.
    pub subagent_mcp_socket: Option<PathBuf>,
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
    pub harness: HarnessKind,
    pub execution_policy: ExecutionPolicy,
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
    pub turn_context: mj_core::activity::verdict::TurnContext,
    /// None resolves the optional classifier from the local environment.
    pub verdict: Option<VerdictSource>,
    pub stall_policy: Option<mj_core::activity::StallPolicy>,
}

pub(super) fn project_memory_mcp(spec: &LaunchSpec) -> Vec<McpServer> {
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
    if spec.harness == HarnessKind::Muse
        || (spec.harness == HarnessKind::Claude && memory.history_socket.is_none())
    {
        return Vec::new();
    }
    let mut args = vec![
        "worker".into(),
        "memory-mcp".into(),
        "--root".into(),
        memory.root.to_string_lossy().into_owned(),
    ];
    if let Some(socket) = &memory.history_socket {
        args.extend([
            "--history-socket".into(),
            socket.to_string_lossy().into_owned(),
        ]);
    }
    if spec.harness == HarnessKind::Claude {
        args.push("--native-notes".into());
    }
    vec![McpServer::Stdio(
        McpServerStdio::new("mj-memory", spec.command.clone()).args(args),
    )]
}

pub(super) fn session_request_meta(
    spec: &LaunchSpec,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if spec.harness == HarnessKind::Codex {
        let asking = spec
            .goal_recovery
            .lock()
            .expect("goal lock poisoned")
            .asking();
        let mut meta = serde_json::Map::from_iter([(
            "goal".into(),
            serde_json::json!({"resumePolicy": if asking {"pause"} else {"preserve"}}),
        )]);
        // Hide the harness's own delegation tool only when Mjolnir replaced
        // it. The socket exists exactly when the controller asked for Mjolnir
        // sub-agents, so the two halves cannot disagree.
        if spec.subagent_mcp_socket.is_some() {
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
    let mut claude_code = serde_json::Map::from_iter([(
        "emitRawSDKMessages".to_owned(),
        serde_json::json!([{
            "type": "system",
            "subtype": CLAUDE_BACKGROUND_TASKS_CHANGED_SUBTYPE,
        }]),
    )]);
    let mut options = serde_json::Map::from_iter([(
        "perTaskStopAffordance".to_owned(),
        serde_json::Value::Bool(true),
    )]);
    // Claude builds its resume catalogue before ACP selector restoration. With
    // setup-token auth, only an explicit startup pin retains the 1M model row.
    if let Some(model) = &spec
        .accepted_config
        .lock()
        .expect("accepted session configuration lock poisoned")
        .model
    {
        options.insert("model".to_owned(), serde_json::Value::String(model.clone()));
    }
    if let Some(enabled) = spec
        .harness
        .execution_enforcement(spec.execution_policy)
        .and_then(mj_core::config::ExecutionEnforcement::session_sandbox)
    {
        options.insert(
            "sandbox".to_owned(),
            serde_json::json!({ "enabled": enabled }),
        );
    }
    // Same rule as Codex above: without the Mjolnir delegation socket the
    // session keeps Claude's own Agent and Task tools.
    if spec.subagent_mcp_socket.is_some() {
        options.insert(
            "disallowedTools".to_owned(),
            serde_json::json!(["Agent", "Task", "TaskOutput", "TaskStop"]),
        );
    }
    claude_code.insert("options".to_owned(), serde_json::Value::Object(options));
    Some(serde_json::Map::from_iter([(
        "claudeCode".to_owned(),
        serde_json::Value::Object(claude_code),
    )]))
}

/// The reviewing agents' analyzer servers, for harnesses that accept a server
/// over ACP. Claude and Kimi read their staged profile instead, which the
/// controller writes while staging the reviewer.
pub(super) fn extra_mcp(spec: &LaunchSpec) -> Vec<McpServer> {
    let mut servers = if mj_core::worker_launch::ReviewMcpDelivery::for_harness(spec.harness)
        == mj_core::worker_launch::ReviewMcpDelivery::Acp
    {
        spec.extra_mcp_servers
            .iter()
            .map(|server| {
                McpServer::Stdio(
                    McpServerStdio::new(server.name.clone(), server.command.clone())
                        .args(server.args.clone()),
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    if spec.harness != HarnessKind::Claude
        && let Some(socket) = &spec.subagent_mcp_socket
    {
        let worker = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("hel"));
        servers.push(McpServer::Stdio(
            McpServerStdio::new("mj-agents", worker).args(vec![
                "worker".into(),
                "subagent-mcp".into(),
                "--socket".into(),
                socket.to_string_lossy().into_owned(),
                // The server bounds a `wait` by what this harness's own MCP
                // client will hold open.
                "--harness".into(),
                spec.harness.id().to_owned(),
            ]),
        ));
    }
    servers
}

fn session_mcp(spec: &LaunchSpec, include_project_memory: bool) -> Vec<McpServer> {
    let mut servers = extra_mcp(spec);
    if include_project_memory {
        servers.extend(project_memory_mcp(spec));
    }
    servers
}

pub(super) fn new_session_request(
    spec: &LaunchSpec,
    include_project_memory: bool,
) -> NewSessionRequest {
    let request = NewSessionRequest::new(spec.cwd.clone())
        .additional_directories(spec.additional_directories.clone())
        .meta(session_request_meta(spec));
    request.mcp_servers(session_mcp(spec, include_project_memory))
}

pub(super) fn load_session_request(spec: &LaunchSpec, session_id: SessionId) -> LoadSessionRequest {
    LoadSessionRequest::new(session_id, spec.cwd.clone())
        .additional_directories(spec.additional_directories.clone())
        // The servers Mjolnir owns travel on every launch, because a bridge
        // that opens the session again is a new harness process: Codex builds
        // the resumed thread's MCP set from this request and recovers nothing
        // it was not given (#1085). Replay filtering belongs to the notification
        // handler; omitting memory here removes its tools after restart.
        .mcp_servers(session_mcp(spec, true))
        .meta(session_request_meta(spec))
}

pub(super) fn resume_session_request(
    spec: &LaunchSpec,
    session_id: SessionId,
) -> ResumeSessionRequest {
    ResumeSessionRequest::new(session_id, spec.cwd.clone())
        .additional_directories(spec.additional_directories.clone())
        .mcp_servers(session_mcp(spec, true))
        .meta(session_request_meta(spec))
}

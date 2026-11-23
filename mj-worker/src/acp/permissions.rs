use super::*;
use agent_client_protocol::v1::schema::{PermissionOption, PermissionOptionId};

/// A permission callback captures the current command's sender, so a late
/// answer cannot attach an implementation to a subsequent prompt or bridge.
pub(super) type PlanImplementationSlot =
    Arc<Mutex<Option<mpsc::UnboundedSender<PlanImplementation>>>>;

pub(super) struct PlanImplementation {
    pub(super) plan: String,
    pub(super) permission_sent: oneshot::Receiver<bool>,
}

pub(super) struct ActivePlanImplementation(pub(super) PlanImplementationSlot);

pub(super) struct RestoredPlanMode {
    pub(super) config_options: Vec<SessionConfigOption>,
    pub(super) modes: Option<SessionModeState>,
    pub(super) plan: String,
    /// Set when Claude refused the policy mode and Plan was left for its
    /// default mode instead.
    pub(super) warning: Option<String>,
}

pub(super) fn current_session_mode(
    config_options: &[SessionConfigOption],
    modes: &Option<SessionModeState>,
) -> Option<String> {
    config_options
        .iter()
        .find(|option| option.category == Some(SessionConfigOptionCategory::Mode))
        .and_then(|option| surface::config_current_value(config_options, &option.id.to_string()))
        .or_else(|| surface::config_current_value(config_options, "mode"))
        .or_else(|| {
            modes
                .as_ref()
                .map(|state| state.current_mode_id.to_string())
        })
}

pub(super) type PlanModeRestoration<'a> =
    Pin<Box<dyn Future<Output = Result<RestoredPlanMode>> + Send + 'a>>;

pub(super) async fn restore_plan_execution_mode(
    connection: &ConnectionTo<Agent>,
    session_id: SessionId,
    mut state: RestoredPlanMode,
    permission_sent: oneshot::Receiver<bool>,
    enforcement: Option<ExecutionEnforcement>,
) -> Result<RestoredPlanMode> {
    ensure!(
        permission_sent.await.unwrap_or(false),
        "Claude's plan permission response could not be delivered"
    );
    state.warning = leave_plan_for_policy(
        connection,
        &session_id,
        HarnessKind::Claude,
        enforcement,
        &mut state.config_options,
        &mut state.modes,
    )
    .await?;
    Ok(state)
}

pub(super) enum PlanPermissionAnswer {
    Native(RequestPermissionResponse),
    ContinueInBypass,
}

/// The Claude permission mode the session's enforcement selects: the
/// profile's `claude_permission_mode` (Auto by default) for Guardian,
/// bypassPermissions for YOLO. It remains the fallback for a plan approval
/// that did not enter through a recorded Mjolnir Plan control.
fn claude_policy_mode(enforcement: Option<ExecutionEnforcement>) -> &'static str {
    enforcement
        .and_then(ExecutionEnforcement::acp_mode)
        .expect("Claude enforces an ACP mode under every execution policy")
}

/// The permission mode a Claude plan-approval option continues in, and whether
/// it first clears the context. These IDs come from claude-agent-acp 0.108.0.
fn claude_plan_option_mode(option_id: &str) -> Option<(&'static str, bool)> {
    Some(match option_id {
        "exit-plan-bypass" => ("bypassPermissions", false),
        "exit-plan-auto" => ("auto", false),
        "exit-plan-accept-edits" => ("acceptEdits", false),
        "exit-plan-default" => ("default", false),
        "exit-plan-clear-bypass" => ("bypassPermissions", true),
        "exit-plan-clear-auto" => ("auto", true),
        "exit-plan-clear-accept-edits" => ("acceptEdits", true),
        _ => return None,
    })
}

/// The option that approves a Claude plan and continues under the session's
/// own execution policy. Plan review's Implement and the approval form's Yes
/// both select it.
fn claude_policy_plan_option(
    request: &RequestPermissionRequest,
    enforcement: Option<ExecutionEnforcement>,
) -> Option<&PermissionOption> {
    let mode = claude_policy_mode(enforcement);
    request.options.iter().find(|option| {
        claude_plan_option_mode(&option.option_id.0) == Some((mode, false))
            && matches!(
                option.kind,
                PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
            )
    })
}

/// One answer a permission form offers, and the harness option it selects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PermissionChoice {
    pub(super) option_id: PermissionOptionId,
    pub(super) title: String,
}

/// The answers a person may give to a permission request shown as a form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PermissionAnswers {
    pub(super) choices: Vec<PermissionChoice>,
    /// The option Skip selects: the first offered one-time rejection, so the
    /// agent is refused this tool call and its turn continues. Skip never
    /// selects `reject_always`, which would persist a rule. Without a
    /// one-time rejection the form offers no Skip.
    pub(super) skip: Option<PermissionOptionId>,
}

/// The answers a person may give to a permission request shown as a form.
///
/// This is the only place that decides them: the worker publishes exactly
/// these choices and whether Skip is offered, and accepts only these
/// answers, so every surface (terminal, web, `mj respond`, automation) sees
/// and can select the same set.
///
/// Claude's plan approval form preserves the session's execution policy. The
/// option that continues under that policy becomes a plain "Yes" and hides
/// other modes. When Claude omits the policy's mode, lower modes stay available
/// for approval. Guardian keeps bypassPermissions hidden.
pub(super) fn permission_choices(
    request: &RequestPermissionRequest,
    plan_permission: Option<PlanPermission>,
    enforcement: Option<ExecutionEnforcement>,
) -> PermissionAnswers {
    let choices = offered_permission_choices(request, plan_permission, enforcement);
    let skip = request
        .options
        .iter()
        .filter(|option| option.kind == PermissionOptionKind::RejectOnce)
        .find(|option| {
            choices
                .iter()
                .any(|choice| choice.option_id == option.option_id)
        })
        .map(|option| option.option_id.clone());
    PermissionAnswers { choices, skip }
}

fn offered_permission_choices(
    request: &RequestPermissionRequest,
    plan_permission: Option<PlanPermission>,
    enforcement: Option<ExecutionEnforcement>,
) -> Vec<PermissionChoice> {
    if plan_permission != Some(PlanPermission::Claude) {
        return request
            .options
            .iter()
            .map(|option| PermissionChoice {
                option_id: option.option_id.clone(),
                title: option.name.clone(),
            })
            .collect();
    }
    let policy_mode = claude_policy_mode(enforcement);
    let yes = claude_policy_plan_option(request, enforcement);
    // Any option in the policy's mode, its clear-context one included, lets
    // the plan be approved without another mode.
    let policy_offered = request.options.iter().any(|option| {
        claude_plan_option_mode(&option.option_id.0).is_some_and(|(mode, _)| mode == policy_mode)
    });
    let mut choices: Vec<_> = yes
        .map(|option| PermissionChoice {
            option_id: option.option_id.clone(),
            title: "Yes".into(),
        })
        .into_iter()
        .collect();
    for option in &request.options {
        if yes.is_some_and(|yes| yes.option_id == option.option_id) {
            continue;
        }
        let mode = claude_plan_option_mode(&option.option_id.0);
        let title = match mode {
            // The policy owns the mode: with its own mode on offer, no other
            // mode is. Without it, lower modes stay so the plan can still be
            // approved.
            Some((mode, _)) if policy_offered && mode != policy_mode => continue,
            Some(("bypassPermissions", _)) if policy_mode != "bypassPermissions" => continue,
            Some((mode, true)) if mode == policy_mode => "Yes, clear context".to_owned(),
            _ => option.name.clone(),
        };
        choices.push(PermissionChoice {
            option_id: option.option_id.clone(),
            title,
        });
    }
    choices
}

const CODEX_IMPLEMENT_PLAN: &str = "implement_plan";
const CODEX_REVISE_PLAN: &str = "revise_plan";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanPermission {
    Claude,
    Codex,
}

impl PlanPermission {
    pub(super) fn can_implement(
        self,
        request: &RequestPermissionRequest,
        enforcement: Option<ExecutionEnforcement>,
    ) -> bool {
        match self {
            Self::Codex => true,
            Self::Claude => {
                claude_policy_plan_option(request, enforcement).is_some()
                    || claude_policy_mode(enforcement) == "bypassPermissions"
            }
        }
    }

    pub(super) fn recognize(
        request: &RequestPermissionRequest,
        harness: HarnessKind,
    ) -> Option<Self> {
        match harness {
            // Claude's AIR tool call omits the tool kind. The ExitPlanMode
            // options identify the approval.
            HarnessKind::Claude
                if request
                    .options
                    .iter()
                    .any(|option| claude_plan_option_mode(&option.option_id.0).is_some()) =>
            {
                Some(Self::Claude)
            }
            HarnessKind::Codex
                if permission_option(request, CODEX_IMPLEMENT_PLAN).is_some()
                    && permission_option(request, CODEX_REVISE_PLAN).is_some() =>
            {
                Some(Self::Codex)
            }
            _ => None,
        }
    }

    pub(super) fn answer(
        self,
        request: &RequestPermissionRequest,
        response: ElicitationResponse,
        enforcement: Option<ExecutionEnforcement>,
    ) -> Result<PlanPermissionAnswer> {
        if plan_review_answer(response).0 != "implement" {
            return Ok(PlanPermissionAnswer::Native(self.decline(request)));
        }
        if self == Self::Codex {
            return Ok(PlanPermissionAnswer::Native(select_plan_option(
                permission_option(request, CODEX_IMPLEMENT_PLAN),
            )));
        }
        if let Some(option) = claude_policy_plan_option(request, enforcement) {
            return Ok(PlanPermissionAnswer::Native(select_plan_option(Some(
                option,
            ))));
        }
        let mode = claude_policy_mode(enforcement);
        if mode == "bypassPermissions" {
            Ok(PlanPermissionAnswer::ContinueInBypass)
        } else {
            bail!(
                "Cannot implement the approved plan: Claude did not offer the required {mode} mode. Update the Claude bridge, use a model supporting that mode, or choose another Claude permission mode in the profile's settings."
            )
        }
    }

    pub(super) fn decline(self, request: &RequestPermissionRequest) -> RequestPermissionResponse {
        let option = match self {
            Self::Codex => permission_option(request, CODEX_REVISE_PLAN),
            Self::Claude => permission_option(request, "reject").or_else(|| {
                [
                    PermissionOptionKind::RejectOnce,
                    PermissionOptionKind::RejectAlways,
                ]
                .into_iter()
                .find_map(|kind| {
                    request.options.iter().find(|option| {
                        option.kind == kind
                            && claude_plan_option_mode(&option.option_id.0).is_none()
                    })
                })
            }),
        };
        select_plan_option(option)
    }
}

fn permission_option<'a>(
    request: &'a RequestPermissionRequest,
    id: &str,
) -> Option<&'a PermissionOption> {
    request
        .options
        .iter()
        .find(|option| option.option_id.0.as_ref() == id)
}

fn select_plan_option(option: Option<&PermissionOption>) -> RequestPermissionResponse {
    RequestPermissionResponse::new(
        option.map_or(RequestPermissionOutcome::Cancelled, |option| {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            ))
        }),
    )
}

/// The answer that allows a tool call the worker has already authorized,
/// preferably for this call alone. Harnesses can still ask for tool approval
/// after their full-access mode was selected, and YOLO authorizes those
/// requests; [`OwnedMcpApprovals`] authorizes calls to Mjolnir's own servers.
pub(super) fn allow_permission_response(
    request: &RequestPermissionRequest,
) -> Option<RequestPermissionResponse> {
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
        .map(|option| {
            RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new(option.option_id.clone()),
            ))
        })
}

/// Authorize only an execute tool with a single literal gh invocation. Titles
/// and display summaries are never evidence of what will execute.
pub(super) fn gh_permission_response(
    request: &RequestPermissionRequest,
    input: Option<&serde_json::Value>,
) -> Option<RequestPermissionResponse> {
    if request.tool_call.fields.kind != Some(agent_client_protocol::v1::schema::ToolKind::Execute) {
        return None;
    }
    let input = input?.as_object()?;
    // Conflicting command representations are ambiguous.
    let command = match (input.get("command"), input.get("cmd")) {
        (Some(command), None) | (None, Some(command)) => command,
        _ => return None,
    };
    // DSH's run_command transports the executable separately from argv.
    let normalized;
    let command = if let Some(args) = input.get("args") {
        let executable = command.as_str()?;
        let args = args.as_array()?;
        if args.len() > 1023 || executable.len() > 16 * 1024 {
            return None;
        }
        let mut size = executable.len();
        for arg in args {
            size = size.checked_add(arg.as_str()?.len())?;
            if size > 16 * 1024 {
                return None;
            }
        }
        normalized = serde_json::Value::Array(
            std::iter::once(serde_json::Value::String(executable.to_owned()))
                .chain(args.iter().cloned())
                .collect(),
        );
        &normalized
    } else {
        command
    };
    let authorized = match command {
        serde_json::Value::String(script) => gh_script(script),
        serde_json::Value::Array(argv) if argv.len() <= 1024 => {
            let argv = argv
                .iter()
                .map(serde_json::Value::as_str)
                .collect::<Option<Vec<_>>>()?;
            if argv.iter().map(|arg| arg.len()).sum::<usize>() > 16 * 1024 {
                return None;
            }
            match argv.as_slice() {
                ["gh", ..] => true,
                [shell, "-c" | "-lc", script]
                    if matches!(
                        *shell,
                        "bash" | "sh" | "/bin/bash" | "/bin/sh" | "/usr/bin/bash" | "/usr/bin/sh"
                    ) =>
                {
                    gh_script(script)
                }
                _ => false,
            }
        }
        _ => false,
    };
    authorized
        .then(|| allow_permission_response(request))
        .flatten()
}

fn gh_script(script: &str) -> bool {
    if script.len() > 16 * 1024 {
        return false;
    }
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        return false;
    }
    let Some(tree) = parser.parse(script, None) else {
        return false;
    };
    let root = tree.root_node();
    if root.has_error() || root.named_child_count() != 1 {
        return false;
    }
    let Some(command) = root.named_child(0) else {
        return false;
    };
    if command.kind() != "command" || command.named_child_count() < 1 {
        return false;
    }
    // Reject terminators too: the complete script is exactly the command.
    if script.trim() != &script[command.byte_range()] {
        return false;
    }
    let Some(name) = command.child_by_field_name("name") else {
        return false;
    };
    if &script[name.byte_range()] != "gh" {
        return false;
    }
    fn literal(node: tree_sitter::Node<'_>, depth: usize) -> bool {
        if depth > 16
            || !matches!(
                node.kind(),
                "command"
                    | "command_name"
                    | "word"
                    | "number"
                    | "string"
                    | "string_content"
                    | "raw_string"
                    | "concatenation"
            )
        {
            return false;
        }
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .all(|child| literal(child, depth + 1))
    }
    literal(command, 0)
}

/// Calls to Mjolnir's own MCP servers, for a harness that cannot be told to
/// trust them. Under configured approvals DSH asks before every MCP tool call
/// and has no per-server rule, so the worker allows the tools the servers this
/// launch registered itself list, as Claude's allowed tools and Codex's server
/// approval mode do for those harnesses. Every other request still reaches a
/// person. A title is matched against each owned tool's full name, never a
/// server prefix, and DSH's double separator keeps server and tool apart.
///
/// OpenCode gets no such answers. It titles a call `<server>_<tool>`, and
/// server names may contain `_`, so another server (for example one a
/// project's own configuration adds, named `mj-agents_send` with a `message`
/// tool) can produce exactly an owned title. Nothing in the request says which
/// server a call belongs to, so its person decides, and can allow always.
///
/// Antigravity's `mcp_<server>_<tool>` titles are ambiguous in the same way,
/// so it gets no answers here either. agy has a grant that names the server,
/// which the worker stages instead (`stage_antigravity_grants`).
#[derive(Debug, Clone, Default)]
pub(super) struct OwnedMcpApprovals {
    /// The permission title of each owned server's tool calls, without any
    /// argument the harness appends.
    titles: Vec<String>,
    /// Whether the harness follows the title with a space and an argument.
    appends_argument: bool,
}

impl OwnedMcpApprovals {
    pub(super) fn for_launch(spec: &LaunchSpec) -> Self {
        if spec.execution_policy != ExecutionPolicy::ConfiguredApprovals {
            return Self::default();
        }
        // dsh-acp names an MCP tool `mcp__<server>__<tool>` and titles its
        // call with that name, underscores shown as spaces, then any `path`
        // argument.
        if spec.harness != HarnessKind::Dsh {
            return Self::default();
        }
        let titles = owned_acp_mcp_tools(spec)
            .iter()
            .flat_map(|(server, tools)| {
                tools
                    .iter()
                    .map(move |tool| format!("mcp__{server}__{tool}").replace('_', " "))
            })
            .collect();
        Self {
            titles,
            appends_argument: true,
        }
    }

    /// The allowing answer when `request` is a call to an owned tool.
    pub(super) fn response(
        &self,
        request: &RequestPermissionRequest,
    ) -> Option<RequestPermissionResponse> {
        let title = request.tool_call.fields.title.as_deref()?;
        let owned = self.titles.iter().any(|owned| {
            title == owned
                || (self.appends_argument
                    // One space then the argument; more spaces mean a longer
                    // server or tool name that merely begins with an owned one.
                    && title
                        .strip_prefix(owned.as_str())
                        .and_then(|rest| rest.strip_prefix(' '))
                        .is_some_and(|argument| !argument.is_empty() && !argument.starts_with(' ')))
        });
        if owned {
            allow_permission_response(request)
        } else {
            None
        }
    }
}

/// Taking the pending entry decides whether answering or withdrawal wins.
/// Once a human or AFK answer was accepted, cancellation still receives it
/// so its resolution is recorded even when both notifications are ready.
pub(super) async fn await_elicitation_response(
    pending: &PendingElicitations,
    id: &str,
    mut answer: oneshot::Receiver<PendingAnswer>,
    cancellation: impl std::future::Future<Output = ()>,
) -> Option<PendingAnswer> {
    tokio::select! {
        biased;
        () = cancellation => {
            let withdrawn = pending.remove(id);
            if withdrawn.is_some() {
                None
            } else {
                answer.await.ok()
            }
        }
        response = async {
            tokio::select! {
                response = &mut answer => response.ok(),
                () = pending.refine_question(id) => (&mut answer).await.ok(),
            }
        } => response,
    }
}

pub(super) fn resolve_pending_elicitation(
    pending: &PendingElicitations,
    elicitation_id: &str,
    response: ElicitationResponse,
) -> std::result::Result<(), String> {
    pending.resolve(elicitation_id, response)
}

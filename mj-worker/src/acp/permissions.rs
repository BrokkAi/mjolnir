use super::*;
use agent_client_protocol::schema::v1::{PermissionOption, PermissionOptionId};

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
    pub(super) transition: PlanModeTransition,
}

/// Tracks the mode Claude had before Plan, including an explicit mode change
/// made while planning so plan completion cannot overwrite the user's choice.
#[derive(Debug, Clone, Default)]
pub(super) enum PlanModeTransition {
    #[default]
    Inactive,
    Planning {
        restore_mode: Option<String>,
    },
    ExplicitModeChange,
}

impl PlanModeTransition {
    pub(super) fn mode_applied(&mut self, mode: &str, previous: Option<String>) {
        if mode == "plan" {
            if previous.as_deref() != Some("plan") {
                *self = Self::Planning {
                    restore_mode: previous,
                };
            }
        } else if matches!(self, Self::Planning { .. }) {
            *self = Self::ExplicitModeChange;
        }
    }
}

pub(super) fn current_session_mode(
    config_options: &[SessionConfigOption],
    modes: &Option<SessionModeState>,
) -> Option<String> {
    surface::config_current_value(config_options, "mode").or_else(|| {
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
) -> Result<RestoredPlanMode> {
    ensure!(
        permission_sent.await.unwrap_or(false),
        "Claude's plan permission response could not be delivered"
    );
    let transition = state.transition.clone();
    let desired = match transition {
        PlanModeTransition::Planning {
            restore_mode: Some(mode),
        } => mode,
        PlanModeTransition::Planning { restore_mode: None } => {
            bail!("Claude's mode before Plan was not reported")
        }
        PlanModeTransition::ExplicitModeChange => {
            state.transition = PlanModeTransition::default();
            return Ok(state);
        }
        PlanModeTransition::Inactive => "bypassPermissions".to_owned(),
    };
    enforce_execution_mode(
        connection,
        &session_id,
        HarnessKind::Claude,
        None,
        &desired,
        &mut state.config_options,
        &mut state.modes,
    )
    .await?;
    state.transition = PlanModeTransition::default();
    Ok(state)
}

pub(super) enum PlanPermissionAnswer {
    Native(RequestPermissionResponse),
    ContinueInBypass,
}

/// The Claude permission mode the session's execution policy enforces: Auto
/// for Guardian, bypassPermissions for YOLO. It remains the fallback for a
/// plan approval that did not enter through a recorded Mjolnir Plan control.
fn claude_policy_mode(policy: ExecutionPolicy) -> &'static str {
    HarnessKind::Claude
        .execution_enforcement(policy)
        .and_then(ExecutionEnforcement::acp_mode)
        .expect("Claude enforces an ACP mode under every execution policy")
}

/// The permission mode a Claude plan-approval option continues in, and whether
/// it first clears the context. claude-agent-acp 0.84 names the options
/// `exit-plan-*`; earlier bridges named them by the mode itself.
fn claude_plan_option_mode(option_id: &str) -> Option<(&'static str, bool)> {
    Some(match option_id {
        "exit-plan-bypass" | "bypassPermissions" => ("bypassPermissions", false),
        "exit-plan-auto" | "auto" => ("auto", false),
        "exit-plan-accept-edits" | "acceptEdits" => ("acceptEdits", false),
        "exit-plan-default" | "default" => ("default", false),
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
    policy: ExecutionPolicy,
) -> Option<&PermissionOption> {
    let mode = claude_policy_mode(policy);
    request.options.iter().find(|option| {
        claude_plan_option_mode(&option.option_id.to_string()) == Some((mode, false))
            && matches!(
                option.kind,
                PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
            )
    })
}

/// Whether this is Claude's own ExitPlanMode approval in the claude-agent-acp
/// 0.84 shape. Its tool call carries no kind for an AIR client, so the option
/// ids are what identify it.
pub(super) fn is_claude_plan_approval(
    request: &RequestPermissionRequest,
    harness: HarnessKind,
) -> bool {
    harness == HarnessKind::Claude
        && request
            .options
            .iter()
            .any(|option| option.option_id.to_string().starts_with("exit-plan-"))
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
/// Claude's own plan approval (claude-agent-acp 0.84 `exit-plan-*` options,
/// which reach this form when Claude wrote no plan file) must not change the
/// session's execution policy. The option that continues under that policy
/// becomes a plain "Yes"; an option that would raise a Guardian session to
/// bypassPermissions is not offered at all. Lower modes stay available.
pub(super) fn permission_choices(
    request: &RequestPermissionRequest,
    harness: HarnessKind,
    policy: ExecutionPolicy,
) -> PermissionAnswers {
    let choices = offered_permission_choices(request, harness, policy);
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
    harness: HarnessKind,
    policy: ExecutionPolicy,
) -> Vec<PermissionChoice> {
    if !is_claude_plan_approval(request, harness) {
        return request
            .options
            .iter()
            .map(|option| PermissionChoice {
                option_id: option.option_id.clone(),
                title: option.name.clone(),
            })
            .collect();
    }
    let policy_mode = claude_policy_mode(policy);
    let yes = claude_policy_plan_option(request, policy);
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
        let mode = claude_plan_option_mode(&option.option_id.to_string());
        let title = match mode {
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

pub(super) fn policy_plan_permission_answer(
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
    let mode = claude_policy_mode(policy);
    if let Some(option) = claude_policy_plan_option(request, policy) {
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

pub(super) fn nested_string_matches(
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

pub(super) fn is_plan_permission(request: &RequestPermissionRequest) -> bool {
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

pub(super) fn permission_plan_response(
    request: &RequestPermissionRequest,
    response: ElicitationResponse,
) -> RequestPermissionResponse {
    let (action, _) = plan_review_answer(response);
    let needles: &[&str] = match action.as_str() {
        "implement" => &["implement_plan", "plan_approve", "default", "approve"],
        "revise" => &["plan_revise", "revise"],
        "exit" => &["reject_and_exit", "exit"],
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
    if request.tool_call.fields.kind != Some(agent_client_protocol::schema::v1::ToolKind::Execute) {
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
/// and has no per-server rule, so the worker allows calls to the servers this
/// launch registered itself, as Claude's allowed tools and Codex's server
/// approval mode do for those harnesses. Every other request still reaches a
/// person.
#[derive(Debug, Clone, Default)]
pub(super) struct OwnedMcpApprovals {
    /// The start of the permission title of each owned server's tool calls.
    title_prefixes: Vec<String>,
}

impl OwnedMcpApprovals {
    pub(super) fn for_launch(spec: &LaunchSpec) -> Self {
        if spec.harness != HarnessKind::Dsh
            || spec.execution_policy != ExecutionPolicy::ConfiguredApprovals
        {
            return Self::default();
        }
        // dsh-acp names an MCP tool `mcp__<server>__<tool>` and titles its
        // call with that name, underscores shown as spaces, then any `path`
        // argument.
        let title_prefixes = owned_acp_mcp_server_names(spec)
            .into_iter()
            .map(|server| format!("mcp__{server}__").replace('_', " "))
            .collect();
        Self { title_prefixes }
    }

    /// The allowing answer when `request` is a call to an owned server.
    pub(super) fn response(
        &self,
        request: &RequestPermissionRequest,
    ) -> Option<RequestPermissionResponse> {
        let title = request.tool_call.fields.title.as_deref()?;
        let owned = self.title_prefixes.iter().any(|prefix| {
            // No owned tool name starts with an underscore, so a space here
            // means a longer server name, such as `mj-agents_`, that merely
            // begins with an owned one.
            title
                .strip_prefix(prefix.as_str())
                .is_some_and(|tool| !tool.is_empty() && !tool.starts_with(' '))
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
        response = &mut answer => response.ok(),
    }
}

pub(super) fn resolve_pending_elicitation(
    pending: &PendingElicitations,
    elicitation_id: &str,
    response: ElicitationResponse,
) -> std::result::Result<(), String> {
    pending.resolve(elicitation_id, response)
}

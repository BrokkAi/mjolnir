use super::*;
use agent_client_protocol::schema::v1::{PermissionOption, PermissionOptionId};

pub(super) type PendingElicitations = Arc<Mutex<BTreeMap<String, PendingElicitation>>>;

/// A question the worker published and is waiting to have answered. It keeps
/// the question so an answer is checked against what was actually offered.
pub(super) struct PendingElicitation {
    pub(super) question: ElicitationRequest,
    pub(super) answer: oneshot::Sender<ElicitationResponse>,
}

#[cfg(test)]
impl PendingElicitation {
    /// A pending question with no fields, for tests that only need it open.
    pub(super) fn open(id: &str, answer: oneshot::Sender<ElicitationResponse>) -> Self {
        Self {
            question: ElicitationRequest {
                id: id.to_owned(),
                message: String::new(),
                title: None,
                description: None,
                fields: Vec::new(),
            },
            answer,
        }
    }
}

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
///
/// This is the only place that decides them: the worker publishes exactly
/// these choices and accepts only these answers, so every surface (terminal,
/// web, `mj respond`, automation) sees and can select the same set.
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

/// Harnesses can still ask for tool approval after their full-access mode was
/// selected. YOLO authorizes those requests, preferably for this call alone.
pub(super) fn unconstrained_permission_response(
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

/// Taking the pending entry decides whether answering or withdrawal wins.
/// Once an answer was accepted, cancellation must still receive it so its
/// reply is recorded, even if both notifications are ready at the same time.
pub(super) async fn await_elicitation_response(
    pending: &PendingElicitations,
    id: &str,
    mut answer: oneshot::Receiver<ElicitationResponse>,
    cancellation: impl std::future::Future<Output = ()>,
) -> Option<ElicitationResponse> {
    tokio::select! {
        biased;
        () = cancellation => {
            let withdrawn = pending.lock().expect("pending elicitation lock poisoned").remove(id);
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
    let mut pending = pending.lock().expect("pending elicitation lock poisoned");
    let Some(question) = pending.get(elicitation_id) else {
        return Err(format!(
            "elicitation {elicitation_id:?} is no longer pending"
        ));
    };
    // The worker accepts only an answer to the question it published. A
    // refused answer leaves the question pending for a valid one.
    question.question.validate_response(&response)?;
    let PendingElicitation { answer, .. } = pending
        .remove(elicitation_id)
        .expect("the pending entry was just found");
    answer
        .send(response)
        .map_err(|_| format!("elicitation {elicitation_id:?} was cancelled before it was answered"))
}

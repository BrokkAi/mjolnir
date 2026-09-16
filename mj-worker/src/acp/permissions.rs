use super::*;

pub(super) type PendingElicitations =
    Arc<Mutex<BTreeMap<String, oneshot::Sender<ElicitationResponse>>>>;

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

pub(super) enum PlanPermissionAnswer {
    Native(RequestPermissionResponse),
    ContinueInBypass,
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

pub(super) const UNEXPECTED_PERMISSION_REQUEST_WARNING: &str = "The agent made a permission request while configured to run unconstrained; its execution policy is misconfigured. The request is shown for you to answer.";

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

/// Muse can still ask for individual approval after its allow-all mode was
/// selected. An unconstrained Mjolnir session must answer that protocol edge
/// instead of cancelling it or leaving the adapter parked forever.
pub(super) fn muse_unconstrained_permission_response(
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

pub(super) fn resolve_pending_elicitation(
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

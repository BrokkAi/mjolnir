use super::*;

/// Whether the harness currently lists `value` for `key`.
pub(super) fn selector_value_is_offered(
    options: &[SessionConfigOption],
    key: &str,
    value: &str,
) -> bool {
    find_session_config_option(options, key)
        .is_some_and(|option| select_contains(&option.kind, value))
}

pub(super) fn dropped_selector_warning(
    dropped: &[(&'static str, String)],
    options: &[SessionConfigOption],
) -> String {
    let listed = dropped
        .iter()
        .map(|(key, value)| format!("{key} {value:?}"))
        .collect::<Vec<_>>()
        .join(" and ");
    let current = dropped
        .iter()
        .filter_map(|(key, _)| {
            let option = find_session_config_option(options, key)?;
            let SessionConfigKind::Select(select) = &option.kind else {
                return None;
            };
            Some(format!("{key} {:?}", select.current_value.to_string()))
        })
        .collect::<Vec<_>>()
        .join(" and ");
    let mut message = format!("Could not restore this session's saved {listed}.");
    if !current.is_empty() {
        message.push_str(&format!(" The harness currently reports {current}."));
    }
    message
}

/// The one form that asks the operator to replace every selector this startup
/// had to drop, or `None` when the harness offers no choice for any of them.
pub(super) fn session_config_recovery_request(
    dropped: &[(&'static str, String)],
    options: &[SessionConfigOption],
) -> Option<ElicitationRequest> {
    let fields: Vec<ElicitationField> = dropped
        .iter()
        .filter_map(|(key, _)| {
            let choices = session_config_choices(options, key);
            if choices.is_empty() {
                return None;
            }
            let option = find_session_config_option(options, key);
            Some(ElicitationField {
                id: (*key).to_owned(),
                title: option.map_or_else(|| (*key).to_owned(), |option| option.name.clone()),
                description: None,
                required: true,
                secret: false,
                custom_answer_for: None,
                custom_answer_option: None,
                kind: ElicitationFieldKind::SingleSelect {
                    options: choices
                        .into_iter()
                        .map(|choice| ElicitationOption {
                            value: choice.value,
                            title: choice.name,
                            description: choice.description,
                            preview: None,
                        })
                        .collect(),
                    // Offer the currently reported value when it is selectable.
                    default: option.and_then(|option| match &option.kind {
                        SessionConfigKind::Select(select)
                            if select_contains(&option.kind, &select.current_value.to_string()) =>
                        {
                            Some(select.current_value.to_string())
                        }
                        _ => None,
                    }),
                },
            })
        })
        .collect();
    if fields.is_empty() {
        return None;
    }
    Some(ElicitationRequest {
        id: SESSION_CONFIG_RECOVERY_ID.to_owned(),
        title: Some("Choose a replacement".into()),
        message: dropped_selector_warning(dropped, options),
        description: Some(
            "Pick what this session should use from now on. Declining keeps the current configuration."
                .into(),
        ),
        fields,
        tool_permission: None,
    })
}

/// Answer the recovery question Hel raised for itself.
///
/// Returns `None` when the id belongs to the harness instead, which the caller
/// forwards to the ACP responder waiting on it, and otherwise the answer the
/// caller reports back to whoever submitted the form.
#[allow(clippy::too_many_arguments)]
pub(super) async fn resolve_session_config_recovery(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    spec: &LaunchSpec,
    events: &mpsc::Sender<RuntimeEvent>,
    config_options: &mut Vec<SessionConfigOption>,
    grok_models: &mut Option<grok::GrokModelState>,
    pending: &mut Option<ElicitationRequest>,
    idle: bool,
    elicitation_id: &str,
    response: &ElicitationResponse,
) -> Result<Option<std::result::Result<(), String>>> {
    let Some(request) = pending
        .as_ref()
        .filter(|request| request.id == elicitation_id)
    else {
        return Ok(None);
    };
    if let Err(message) = request.validate_response(response) {
        return Ok(Some(Err(message)));
    }
    let mut chosen: Vec<(&'static str, String)> = Vec::new();
    if let ElicitationResponse::Accept { content } = response {
        // Model before effort: a model change can replace the effort catalogue.
        for key in ["model", "effort"] {
            if !request.fields.iter().any(|field| field.id == key) {
                continue;
            }
            if let Some(ElicitationValue::String(value)) = content.get(key)
                && !value.trim().is_empty()
            {
                chosen.push((key, value.clone()));
            }
        }
    }
    // The same rule the equivalent SetConfig command follows. The question
    // stays pending so the operator can answer it once the turn ends.
    if !idle && !chosen.is_empty() {
        return Ok(Some(Err(
            "configuration can only be changed while the agent is idle".into(),
        )));
    }
    let action = response.action_name().to_owned();
    let reply = Some(request.reply_text(response));
    *pending = None;
    emit_runtime_event(
        events,
        RuntimeEvent::ElicitationResolved {
            elicitation_id: elicitation_id.to_owned(),
            action,
            reply,
        },
    )
    .await?;
    let mut applied = Vec::new();
    let mut refused = Vec::new();
    for (key, value) in chosen {
        match apply_session_selector(
            connection,
            session_id,
            config_options,
            grok_models,
            spec.harness,
            key,
            &value,
        )
        .await
        {
            Ok(value) => {
                spec.accepted_config
                    .lock()
                    .map_err(|_| anyhow!("accepted session configuration lock was poisoned"))?
                    .remember(key, &value, config_options);
                // No request id: this answers Hel's own question, so there is
                // no durable relay command to complete. The worker still
                // records the accepted value, which is what stops the next
                // restart from replaying the one the harness dropped.
                emit_runtime_event(
                    events,
                    RuntimeEvent::ConfigApplied {
                        request_id: String::new(),
                        key: key.to_owned(),
                        value: value.clone(),
                        config_options: config_options.clone(),
                    },
                )
                .await?;
                applied.push(format!("{key} {value:?}"));
            }
            Err(error) => refused.push(format!("{key} {value:?} ({error:#})")),
        }
    }
    let mut message = if applied.is_empty() {
        "This session keeps its current configuration.".to_owned()
    } else {
        format!("This session now uses {}.", applied.join(" and "))
    };
    if !refused.is_empty() {
        message.push_str(&format!(" The harness refused {}.", refused.join(" and ")));
    }
    emit_runtime_event(events, RuntimeEvent::Warning { message }).await?;
    Ok(Some(Ok(())))
}

pub(super) async fn apply_session_selector(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    options: &mut Vec<SessionConfigOption>,
    grok_models: &mut Option<grok::GrokModelState>,
    harness: HarnessKind,
    key: &str,
    value: &str,
) -> Result<String> {
    // Muse advertises its default model as the first choice while reporting an
    // empty current value. Sending that same choice back through its legacy
    // `session/setModel` path is rejected as `invalid_target`; leaving it alone
    // is the only operation needed to select the advertised default.
    if harness == HarnessKind::Muse && key == "model" && muse_implicit_default(options, value) {
        return Ok(value.to_owned());
    }
    match grok_models.as_mut() {
        Some(state) if grok::handles_config_key(key) => {
            grok::apply_model_change(connection, session_id, state, key, value)
                .await
                .inspect(|()| grok::merge_config_options(options, state))
                .map(|()| value.to_owned())
        }
        _ => set_session_config(connection, session_id, options, harness, key, value).await,
    }
}

/// The value to send for a requested selector change, or `None` when the
/// harness should resolve the request itself.
///
/// A listed value is sent as is. A value that matches a listed value or its
/// display name without regard to case selects that value, so `/model` takes
/// what the picker shows. Claude's bridge also resolves full model ids such as
/// `claude-opus-5-5` to the advertised value that runs them (its
/// `resolveModelPreference`), and it advertises no ids of its own to match
/// against, so an unlisted Claude model is left to the bridge, which refuses
/// what it cannot resolve. Any other unlisted value is refused here with the
/// values the harness does accept.
pub(super) fn resolve_selector_value(
    options: &[SessionConfigOption],
    harness: HarnessKind,
    key: &str,
    value: &str,
) -> Result<Option<String>> {
    let option = find_session_config_option(options, key)
        .with_context(|| mj_core::acp::missing_config_selector_refusal(Some(harness), key))?;
    if select_contains(&option.kind, value) {
        return Ok(Some(value.to_owned()));
    }
    let choices = session_config_choices(options, key);
    let wanted = value.trim();
    if let Some(choice) = choices.iter().find(|choice| {
        choice.value.eq_ignore_ascii_case(wanted) || choice.name.eq_ignore_ascii_case(wanted)
    }) {
        return Ok(Some(choice.value.clone()));
    }
    if harness == HarnessKind::Claude && key == "model" && !wanted.is_empty() {
        return Ok(None);
    }
    Err(anyhow!(
        "{value:?} is not an available {key} value; {}",
        accepted_values_sentence(options, key)
    ))
}

fn accepted_values_sentence(options: &[SessionConfigOption], key: &str) -> String {
    let choices = session_config_choices(options, key);
    if choices.is_empty() {
        return "the agent lists no values".to_owned();
    }
    let listed = choices
        .iter()
        .map(|choice| {
            if choice.name.is_empty() || choice.name == choice.value {
                format!("{:?}", choice.value)
            } else {
                format!("{:?} ({})", choice.value, choice.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("choose one of {listed}")
}

pub(super) fn muse_implicit_default(options: &[SessionConfigOption], value: &str) -> bool {
    let Some(option) = find_session_config_option(options, "model") else {
        return false;
    };
    let SessionConfigKind::Select(select) = &option.kind else {
        return false;
    };
    select.current_value.to_string().trim().is_empty()
        && session_config_choices(options, "model")
            .first()
            .is_some_and(|choice| choice.value == value)
}

/// Apply one selector change and return the value the session now uses.
pub(super) async fn set_session_config(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    options: &mut Vec<SessionConfigOption>,
    harness: HarnessKind,
    key: &str,
    value: &str,
) -> Result<String> {
    let resolved = resolve_selector_value(options, harness, key, value)?;
    let option = find_session_config_option(options, key)
        .with_context(|| mj_core::acp::missing_config_selector_refusal(Some(harness), key))?;
    let option_id = option.id.clone();
    let previous = selector_current_value(&option.kind);
    let sent = resolved.clone().unwrap_or_else(|| value.to_owned());
    let response = match connection
        .send_request(SetSessionConfigOptionRequest::new(
            session_id.clone(),
            option_id.clone(),
            SessionConfigValueId::new(sent.clone()),
        ))
        .block_task()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            let error = anyhow::Error::from(error).context(format!("set session {key} to {value}"));
            // The harness refused a value it was left to resolve: say what
            // it does accept, as a value refused locally would.
            return Err(if resolved.is_none() {
                error.context(format!(
                    "{value:?} is not an available {key} value; {}",
                    accepted_values_sentence(options, key)
                ))
            } else {
                error
            });
        }
    };
    // An answer with no configuration at all says nothing about the session.
    // Adopting it would drop every selector the harness advertises, which is
    // worse than a stale value, so the catalogue in hand stays.
    if !response.config_options.is_empty() {
        *options = response.config_options;
    }
    if resolved.is_none() && lands_on_the_default_model(options, &option_id, &sent) {
        // claude-agent-acp resolves full model ids, but its fuzzy last resort
        // places text it cannot resolve on the catch-all `default` entry, so
        // the bridge "accepts" a value it never recognized (R4-1). Select the
        // previous model again and refuse the value as before bc7495e4. A
        // full id that genuinely runs the default model is refused too; the
        // listed `default` value selects it.
        if let Some(previous) = previous
            .as_deref()
            .filter(|previous| !previous.trim().is_empty() && *previous != DEFAULT_MODEL_VALUE)
        {
            let restored = connection
                .send_request(SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    option_id.clone(),
                    SessionConfigValueId::new(previous.to_owned()),
                ))
                .block_task()
                .await
                .with_context(|| {
                    format!("select {key} {previous} again after the bridge placed {value:?} on its default")
                })?;
            if !restored.config_options.is_empty() {
                *options = restored.config_options;
            }
            adopt_requested_value(options, &option_id, Some(DEFAULT_MODEL_VALUE), previous);
        }
        bail!(
            "{value:?} is not an available {key} value; {}",
            accepted_values_sentence(options, key)
        );
    }
    adopt_requested_value(options, &option_id, previous.as_deref(), &sent);
    // A value the harness resolved itself is recorded as the one it now
    // reports, so a restart replays an advertised value and not the alias.
    Ok(match resolved {
        Some(resolved) => resolved,
        None => options
            .iter()
            .find(|option| option.id == option_id)
            .and_then(|option| selector_current_value(&option.kind))
            .filter(|current| !current.trim().is_empty())
            .unwrap_or(sent),
    })
}

/// The value claude-agent-acp advertises for "Default (recommended)".
const DEFAULT_MODEL_VALUE: &str = "default";

/// Whether the harness reports the default model after it was asked for
/// `sent`, a value that is not the default. Only an unlisted Claude model
/// reaches this check: it is the one kind of value left to the bridge.
fn lands_on_the_default_model(
    options: &[SessionConfigOption],
    option_id: &agent_client_protocol::v1::schema::SessionConfigId,
    sent: &str,
) -> bool {
    !sent.trim().eq_ignore_ascii_case(DEFAULT_MODEL_VALUE)
        && options
            .iter()
            .find(|option| &option.id == option_id)
            .and_then(|option| selector_current_value(&option.kind))
            .is_some_and(|current| current == DEFAULT_MODEL_VALUE)
}

fn selector_current_value(kind: &SessionConfigKind) -> Option<String> {
    match kind {
        SessionConfigKind::Select(select) => Some(select.current_value.to_string()),
        _ => None,
    }
}

/// Record the value a successful set actually applied.
///
/// Codex and Kimi answer a successful `session/set_config_option` with the
/// configuration they held before the change, so the selector this call just
/// set still reports its previous value until the harness refreshes it after
/// the next turn. The request succeeded, so the requested value is the applied
/// one and the answer has to say so.
///
/// A harness that reports no value for the selector at all is corrected the
/// same way, for the same reason: it has not named an effective value, and the
/// request it just accepted did.
///
/// Only those cases are corrected. A harness that reports any other value has
/// normalized or replaced the request, so its answer stands and the mismatch
/// stays visible. Every other selector keeps what the harness reported too,
/// because a model change can legitimately reset the rest of the configuration.
fn adopt_requested_value(
    options: &mut [SessionConfigOption],
    option_id: &agent_client_protocol::v1::schema::SessionConfigId,
    previous: Option<&str>,
    value: &str,
) {
    let Some(applied) = options.iter_mut().find(|option| option.id == *option_id) else {
        return;
    };
    if !select_contains(&applied.kind, value) {
        return;
    }
    let SessionConfigKind::Select(select) = &mut applied.kind else {
        return;
    };
    let reported = select.current_value.to_string();
    if reported != value
        && (reported.trim().is_empty() || previous.is_some_and(|previous| previous == reported))
    {
        select.current_value = SessionConfigValueId::new(value.to_owned());
    }
}

/// The mode a harness puts a session in instead of `desired` when the model
/// the session runs cannot use `desired`, and which it announces itself.
///
/// claude-agent-acp 0.81.0 (`dist/session-mode.js`, `AUTO_MODE_FALLBACK`)
/// answers a request for Auto on a model without Auto mode with Accept edits
/// and its "Auto mode unavailable" notice. It does the same when a model
/// change or a reloaded session's model rules Auto out, which Mjolnir already
/// accepted; this lets a mode request that meets the same rule succeed too.
fn announced_mode_substitute(harness: HarnessKind, desired: &str) -> Option<&'static str> {
    match (harness, desired) {
        (HarnessKind::Claude, "auto") => Some("acceptEdits"),
        _ => None,
    }
}

pub(super) async fn enforce_execution_mode(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    harness: HarnessKind,
    selector: Option<&str>,
    desired: &str,
    config_options: &mut Vec<SessionConfigOption>,
    legacy_modes: &mut Option<agent_client_protocol::v1::schema::SessionModeState>,
) -> Result<String> {
    if let Some(option) = mj_core::acp::execution_mode_option(config_options, selector, desired) {
        let option_id = option.id.clone();
        // Legacy session modes mirror only the Mode selector.
        let mirrors_legacy_modes = option.category == Some(SessionConfigOptionCategory::Mode);
        let applied = select_confirmed(
            connection,
            session_id,
            harness,
            option_id,
            "execution mode",
            desired,
            config_options,
        )
        .await?;
        if mirrors_legacy_modes && let Some(modes) = legacy_modes.as_mut() {
            modes.current_mode_id = applied.clone().into();
        }
        return Ok(applied);
    }
    if legacy_modes.as_ref().is_some_and(|modes| {
        modes
            .available_modes
            .iter()
            .any(|mode| mode.id.to_string() == desired)
    }) {
        connection
            .send_request(SetSessionModeRequest::new(
                session_id.clone(),
                desired.to_string(),
            ))
            .block_task()
            .await
            .with_context(|| format!("select required ACP execution mode {desired}"))?;
        // `session/set_mode` answers with an empty result, so the only
        // acknowledgement this path can check is that the request succeeded.
        // Harnesses that report an effective value expose the mode as a
        // config option, which the branch above verifies.
        if let Some(modes) = legacy_modes.as_mut() {
            modes.current_mode_id = desired.to_owned().into();
        }
        return Ok(desired.to_owned());
    }
    bail!("ACP bridge does not expose required execution mode {desired}")
}

/// The ACP enforcement for this launch, honoring the Muse profile's
/// `muse_auto_review` choice as the controller resolved it into the launch
/// environment's `MUSE_APPROVAL_MODE`: `promptUnmatched` keeps the reviewer,
/// `allowAll` runs without permission checks. Guardian Muse always reviews,
/// so the environment only matters while unconstrained.
pub(super) fn session_enforcement(spec: &LaunchSpec) -> Option<ExecutionEnforcement> {
    if spec.harness == HarnessKind::Muse && spec.execution_policy == ExecutionPolicy::Unconstrained
    {
        let review = spec
            .environment
            .get("MUSE_APPROVAL_MODE")
            .is_some_and(|mode| mode == "promptUnmatched");
        spec.harness
            .resolve_execution_enforcement(spec.execution_policy, review)
    } else {
        spec.harness.execution_enforcement(spec.execution_policy)
    }
}

/// Select a config option an execution policy requires besides its mode,
/// such as Muse's auto-review. A bridge without it cannot realize the policy.
pub(super) async fn enforce_policy_setting(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    harness: HarnessKind,
    (key, value): (&str, &str),
    config_options: &mut Vec<SessionConfigOption>,
) -> Result<()> {
    let option_id = config_options
        .iter()
        .find(|option| option.id.to_string() == key && select_contains(&option.kind, value))
        .map(|option| option.id.clone())
        .with_context(|| format!("ACP bridge does not expose required {key} {value}"))?;
    select_confirmed(
        connection,
        session_id,
        harness,
        option_id,
        key,
        value,
        config_options,
    )
    .await
    .map(drop)
}

/// Select `desired` on `option_id` and return the value the harness then
/// reports, refusing any other.
async fn select_confirmed(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    harness: HarnessKind,
    option_id: agent_client_protocol::v1::schema::SessionConfigId,
    label: &str,
    desired: &str,
    config_options: &mut Vec<SessionConfigOption>,
) -> Result<String> {
    let response = connection
        .send_request(SetSessionConfigOptionRequest::new(
            session_id.clone(),
            option_id.clone(),
            SessionConfigValueId::new(desired.to_string()),
        ))
        .block_task()
        .await
        .with_context(|| format!("select required ACP {label} {desired}"))?;
    *config_options = response.config_options;
    // A harness can answer the request and still report another value, so
    // the session is only safe to use once it confirms the effective one.
    let effective = surface::config_current_value(config_options, &option_id.to_string());
    let substitute = announced_mode_substitute(harness, desired);
    match effective.as_deref() {
        Some(value) if value == desired || Some(value) == substitute => Ok(value.to_owned()),
        _ => bail!(
            "the harness acknowledged {label} {desired} but reports {}",
            effective.map_or_else(|| "no value".to_owned(), |value| format!("{value:?}"))
        ),
    }
}

/// The harness's own words when it answered a mode request with an error,
/// or `None` when the failure was not an answer from the harness: a mode it
/// does not list, or an acknowledged request that left another mode.
pub(super) fn mode_refusal(error: &anyhow::Error) -> Option<String> {
    let refusal = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<agent_client_protocol::Error>())?;
    let data = refusal.data.as_ref();
    let text = data
        .and_then(|data| data.get("details").or_else(|| data.get("message")))
        .and_then(serde_json::Value::as_str)
        .unwrap_or(refusal.message.as_str());
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(if text.is_empty() {
        refusal.message.clone()
    } else {
        text
    })
}

/// The one line a session gets when its harness refused the policy's mode
/// for a new session, naming the mode the session is left in.
pub(super) fn refused_mode_warning(
    harness: HarnessKind,
    selector: Option<&str>,
    desired: &str,
    refusal: &str,
    modes: Option<&agent_client_protocol::v1::schema::SessionModeState>,
    config_options: &[SessionConfigOption],
) -> String {
    let harness_name = harness.display_name();
    let current = selector
        .and_then(|id| surface::config_current_value(config_options, id))
        .or_else(|| {
            config_options
                .iter()
                .find(|option| option.category == Some(SessionConfigOptionCategory::Mode))
                .and_then(|option| {
                    surface::config_current_value(config_options, &option.id.to_string())
                })
        })
        .or_else(|| modes.map(|modes| modes.current_mode_id.to_string()));
    let kept = match current {
        Some(id) => {
            let name = modes
                .and_then(|modes| {
                    modes
                        .available_modes
                        .iter()
                        .find(|mode| mode.id.to_string() == id)
                })
                .map(|mode| mode.name.clone())
                .filter(|name| name != &id);
            match name {
                Some(name) => format!("its own mode, {name} ({id})"),
                None => format!("its own mode, {id}"),
            }
        }
        None => "its own mode".to_owned(),
    };
    format!(
        "{harness_name} refused execution mode {desired} for this session ({refusal}), so the \
         session continues in {kept}."
    )
}

/// How `reassert_execution_policy` treats the cached mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PolicySync {
    /// Request the policy values even when the cache already shows them: the
    /// cache can predate a permission response's mode change.
    Always,
    /// Request them only when the cache shows another value, leaving Plan on.
    IfDrifted,
}

/// A harness's answer refusing the policy mode, as `mode_refusal` reads it.
pub(super) struct ModeRefusal {
    pub(super) error: anyhow::Error,
    pub(super) text: String,
}

/// Reconcile the bridge with the launch policy at a command boundary. Plan
/// controls may temporarily select Plan; leaving it always returns to policy.
/// A refused policy mode is the answer, not an error, so the caller decides
/// what the session does instead; the policy setting is selected either way,
/// and any other failure, the setting's included, is an error.
pub(super) async fn reassert_execution_policy(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    harness: HarnessKind,
    enforcement: Option<ExecutionEnforcement>,
    config_options: &mut Vec<SessionConfigOption>,
    modes: &mut Option<SessionModeState>,
    sync: PolicySync,
) -> Result<Option<ModeRefusal>> {
    let Some(enforcement) = enforcement else {
        return Ok(None);
    };
    let mut refused = None;
    if let Some(desired) = enforcement.acp_mode() {
        let current = mj_core::acp::execution_mode_option(
            config_options,
            enforcement.acp_mode_selector(),
            desired,
        )
        .and_then(|option| surface::config_current_value(config_options, &option.id.to_string()))
        .or_else(|| current_session_mode(config_options, modes));
        // A bridge whose catalogue omits the policy mode, as Claude's does
        // for Auto on some models, has refused it as plainly as one that
        // answers the request with an error.
        let offered = mj_core::acp::execution_mode_option(
            config_options,
            enforcement.acp_mode_selector(),
            desired,
        )
        .is_some()
            || modes.as_ref().is_some_and(|modes| {
                modes
                    .available_modes
                    .iter()
                    .any(|mode| mode.id.to_string() == desired)
            });
        let needed = sync == PolicySync::Always
            || (current.as_deref() != Some(desired) && current.as_deref() != Some("plan"));
        if needed && !offered {
            let text = format!(
                "{} does not offer the {desired} mode here",
                harness.display_name()
            );
            refused = Some(ModeRefusal {
                error: anyhow!("{text}"),
                text,
            });
        } else if needed
            && let Err(error) = enforce_execution_mode(
                connection,
                session_id,
                harness,
                enforcement.acp_mode_selector(),
                desired,
                config_options,
                modes,
            )
            .await
        {
            let Some(text) = mode_refusal(&error) else {
                return Err(error);
            };
            refused = Some(ModeRefusal { error, text });
        }
    }
    if let Some((key, value)) = enforcement.acp_setting()
        && (sync == PolicySync::Always
            || surface::config_current_value(config_options, key).as_deref() != Some(value))
    {
        enforce_policy_setting(
            connection,
            session_id,
            harness,
            (key, value),
            config_options,
        )
        .await?;
    }
    Ok(refused)
}

/// The warning a session gets when its harness refused the policy mode,
/// naming the mode the session is left in.
pub(super) fn refused_policy_warning(
    harness: HarnessKind,
    enforcement: Option<ExecutionEnforcement>,
    refusal: &ModeRefusal,
    modes: Option<&SessionModeState>,
    config_options: &[SessionConfigOption],
) -> String {
    let desired = enforcement
        .and_then(ExecutionEnforcement::acp_mode)
        .unwrap_or_default();
    tracing::warn!(
        ?harness,
        mode = desired,
        error = format!("{:#}", refusal.error),
        "the harness refused the execution mode"
    );
    refused_mode_warning(
        harness,
        enforcement.and_then(ExecutionEnforcement::acp_mode_selector),
        desired,
        &refusal.text,
        modes,
        config_options,
    )
}

/// The mode Claude and Kimi leave Plan for when the policy sets no mode, and
/// Claude's when it refuses the policy's: it asks before every change, so it
/// is never looser than the policy, and Plan can always be left.
const PLAN_EXIT_DEFAULT_MODE: &str = "default";

/// Leave Plan for the policy mode, returning the warning to show when the
/// harness refused it and the session took Claude's default mode instead.
/// A policy that sets no mode leaves Plan for the harness default.
pub(super) async fn leave_plan_for_policy(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    harness: HarnessKind,
    enforcement: Option<ExecutionEnforcement>,
    config_options: &mut Vec<SessionConfigOption>,
    modes: &mut Option<SessionModeState>,
) -> Result<Option<String>> {
    if enforcement
        .and_then(ExecutionEnforcement::acp_mode)
        .is_none()
    {
        enforce_execution_mode(
            connection,
            session_id,
            harness,
            None,
            PLAN_EXIT_DEFAULT_MODE,
            config_options,
            modes,
        )
        .await?;
    }
    let Some(refusal) = reassert_execution_policy(
        connection,
        session_id,
        harness,
        enforcement,
        config_options,
        modes,
        PolicySync::Always,
    )
    .await?
    else {
        return Ok(None);
    };
    if harness != HarnessKind::Claude {
        return Err(refusal.error);
    }
    enforce_execution_mode(
        connection,
        session_id,
        harness,
        None,
        PLAN_EXIT_DEFAULT_MODE,
        config_options,
        modes,
    )
    .await?;
    Ok(Some(refused_policy_warning(
        harness,
        enforcement,
        &refusal,
        modes.as_ref(),
        config_options,
    )))
}

/// Reassert the policy after the session's model moved. A harness that
/// refuses the policy mode keeps its own mode with a warning, as at startup:
/// the session stays usable instead of ending.
///
/// A model change can move the bridge's mode, and its answer need not say
/// so (a legacy mode list, or an empty catalogue the worker keeps its own
/// copy over), so the policy is requested outright unless Plan is on.
pub(super) async fn reconcile_execution_policy(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    harness: HarnessKind,
    enforcement: Option<ExecutionEnforcement>,
    config_options: &mut Vec<SessionConfigOption>,
    modes: &mut Option<SessionModeState>,
    events: &mpsc::Sender<RuntimeEvent>,
) -> Result<()> {
    let sync = if current_session_mode(config_options, modes).as_deref() == Some("plan") {
        PolicySync::IfDrifted
    } else {
        PolicySync::Always
    };
    let Some(refusal) = reassert_execution_policy(
        connection,
        session_id,
        harness,
        enforcement,
        config_options,
        modes,
        sync,
    )
    .await?
    else {
        return Ok(());
    };
    let message = refused_policy_warning(
        harness,
        enforcement,
        &refusal,
        modes.as_ref(),
        config_options,
    );
    emit_runtime_event(events, RuntimeEvent::Warning { message }).await
}

/// The model the session reports, which decides whether the bridge allows
/// the policy mode (Claude's Auto).
pub(super) fn session_model(options: &[SessionConfigOption]) -> Option<String> {
    options
        .iter()
        .find(|option| option.category == Some(SessionConfigOptionCategory::Model))
        .and_then(|option| surface::config_current_value(options, &option.id.to_string()))
        .or_else(|| surface::config_current_value(options, "model"))
}

/// Whether a mode change from outside the worker may select `value` on the
/// selector `key` (`None` for the legacy session-mode list). The policy owns
/// its selectors: only Plan and the policy's own values pass, so no caller,
/// an older controller included, can move a session off its policy.
pub(super) fn policy_permits(
    enforcement: Option<ExecutionEnforcement>,
    options: &[SessionConfigOption],
    key: Option<&str>,
    value: &str,
) -> bool {
    let Some(enforcement) = enforcement else {
        return true;
    };
    let policy_mode = |value: &str| value == "plan" || enforcement.acp_mode() == Some(value);
    // The selector the change would reach: `set_session_config` resolves
    // aliases such as `mode` to the bridge's own selector id.
    let resolved = key.map(|key| {
        mj_core::acp::find_session_config_option(options, key)
            .map_or_else(|| key.to_owned(), |option| option.id.to_string())
    });
    match resolved.as_deref() {
        // The legacy mode list mirrors the Mode selector, which carries the
        // policy unless the bridge has the policy's own named selector
        // (Muse images before muse-acp 0.10 do not).
        None => {
            enforcement.acp_mode().is_none()
                || enforcement.acp_mode_selector().is_some_and(|selector| {
                    options
                        .iter()
                        .any(|option| option.id.to_string() == selector)
                })
                || policy_mode(value)
        }
        Some(key)
            if enforcement
                .acp_setting()
                .is_some_and(|(setting, _)| setting == key) =>
        {
            enforcement.acp_setting() == Some((key, value))
        }
        Some(key)
            if enforcement
                .owned_config_keys(options)
                .iter()
                .any(|owned| owned == key) =>
        {
            policy_mode(value)
        }
        Some(_) => true,
    }
}

/// Refusal for a mode change `policy_permits` turns down.
pub(super) const POLICY_OWNED_MODE: &str =
    "The target's permission policy sets this setting. Use plan mode to enter or leave Plan.";

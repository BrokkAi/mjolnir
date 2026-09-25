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
    *pending = None;
    emit_runtime_event(
        events,
        RuntimeEvent::ElicitationResolved {
            elicitation_id: elicitation_id.to_owned(),
            action,
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
        .with_context(|| mj_core::acp::missing_config_selector_refusal(key))?;
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
        .with_context(|| mj_core::acp::missing_config_selector_refusal(key))?;
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
    option_id: &agent_client_protocol::schema::v1::SessionConfigId,
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
    option_id: &agent_client_protocol::schema::v1::SessionConfigId,
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
    desired: &str,
    config_options: &mut Vec<SessionConfigOption>,
    legacy_modes: &mut Option<agent_client_protocol::schema::v1::SessionModeState>,
) -> Result<()> {
    if let Some(option) = config_options.iter().find(|option| {
        option.category == Some(SessionConfigOptionCategory::Mode)
            && select_contains(&option.kind, desired)
    }) {
        let option_id = option.id.to_string();
        let response = connection
            .send_request(SetSessionConfigOptionRequest::new(
                session_id.clone(),
                option.id.clone(),
                SessionConfigValueId::new(desired.to_string()),
            ))
            .block_task()
            .await
            .with_context(|| format!("select required ACP execution mode {desired}"))?;
        *config_options = response.config_options;
        // A harness can answer the request and still report another mode, so
        // the session is only safe to use once it confirms the effective one.
        let effective = surface::config_current_value(config_options, &option_id);
        let substitute = announced_mode_substitute(harness, desired);
        let applied = match effective.as_deref() {
            Some(mode) if mode == desired || Some(mode) == substitute => mode.to_owned(),
            _ => bail!(
                "the harness acknowledged execution mode {desired} but reports {}",
                effective.map_or_else(|| "no mode".to_owned(), |mode| format!("{mode:?}"))
            ),
        };
        if let Some(modes) = legacy_modes.as_mut() {
            modes.current_mode_id = applied.into();
        }
        return Ok(());
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
        return Ok(());
    }
    bail!("ACP bridge does not expose required execution mode {desired}")
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
    desired: &str,
    refusal: &str,
    modes: Option<&agent_client_protocol::schema::v1::SessionModeState>,
    config_options: &[SessionConfigOption],
) -> String {
    let harness_name = harness.display_name();
    let current = config_options
        .iter()
        .find(|option| option.category == Some(SessionConfigOptionCategory::Mode))
        .and_then(|option| surface::config_current_value(config_options, &option.id.to_string()))
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

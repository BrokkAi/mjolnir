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
            Ok(()) => {
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
) -> Result<()> {
    // Muse advertises its default model as the first choice while reporting an
    // empty current value. Sending that same choice back through its legacy
    // `session/setModel` path is rejected as `invalid_target`; leaving it alone
    // is the only operation needed to select the advertised default.
    if harness == HarnessKind::Muse && key == "model" && muse_implicit_default(options, value) {
        return Ok(());
    }
    match grok_models.as_mut() {
        Some(state) if grok::handles_config_key(key) => {
            grok::apply_model_change(connection, session_id, state, key, value)
                .await
                .inspect(|()| grok::merge_config_options(options, state))
        }
        _ => set_session_config(connection, session_id, options, key, value).await,
    }
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

pub(super) async fn set_session_config(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    options: &mut Vec<SessionConfigOption>,
    key: &str,
    value: &str,
) -> Result<()> {
    let option = find_session_config_option(options, key)
        .with_context(|| format!("ACP bridge does not expose a {key} selector"))?;
    ensure!(
        select_contains(&option.kind, value),
        "{value:?} is not an available {key} value"
    );
    let response = connection
        .send_request(SetSessionConfigOptionRequest::new(
            session_id.clone(),
            option.id.clone(),
            SessionConfigValueId::new(value.to_owned()),
        ))
        .block_task()
        .await
        .with_context(|| format!("set session {key} to {value}"))?;
    *options = response.config_options;
    Ok(())
}

pub(super) async fn enforce_execution_mode(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    desired: &str,
    config_options: &mut Vec<SessionConfigOption>,
    legacy_modes: &mut Option<agent_client_protocol::schema::v1::SessionModeState>,
) -> Result<()> {
    if let Some(option) = config_options.iter().find(|option| {
        option.category == Some(SessionConfigOptionCategory::Mode)
            && select_contains(&option.kind, desired)
    }) {
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
        if let Some(modes) = legacy_modes.as_mut() {
            modes.current_mode_id = desired.to_owned().into();
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
        if let Some(modes) = legacy_modes.as_mut() {
            modes.current_mode_id = desired.to_owned().into();
        }
        return Ok(());
    }
    bail!("ACP bridge does not expose required execution mode {desired}")
}

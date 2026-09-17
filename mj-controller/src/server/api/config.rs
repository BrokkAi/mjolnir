use super::*;

pub(super) async fn profile_config(
    State(state): State<ServerState>,
    Path(profile_id): Path<String>,
    Query(query): Query<ProfileConfigQuery>,
) -> Result<Json<mj_core::worker_launch::ProfileConfig>, ApiFailure> {
    crate::server::require_profile(&state.snapshot_rx.borrow(), &profile_id)?;
    let choices = backend(&state)?
        .profile_config(profile_id, query.model, false)
        .await
        .map_err(|error| ApiFailure::unavailable(format!("profile discovery failed: {error:#}")))?;
    Ok(Json(choices))
}

pub(crate) fn validate_selectors(
    choices: &mj_core::worker_launch::ProfileConfig,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<(), ApiFailure> {
    for (key, value, offered) in [
        ("model", model, &choices.models),
        ("effort", effort, &choices.efforts),
    ] {
        if let Some(value) = value
            && !offered.iter().any(|choice| choice.value == value)
        {
            return Err(ApiFailure::bad_request(format!(
                "this profile does not offer {value:?} as {key}; choices: {}",
                offered
                    .iter()
                    .map(|choice| choice.value.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(())
}

pub(super) async fn set_config(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<SetConfigRequest>,
) -> Result<Json<ApiSession>, ApiFailure> {
    validate_action(
        &ControllerAction::SetConfig {
            session_id: session_id.clone(),
            key: request.key.clone(),
            value: request.value.clone(),
        },
        &state.snapshot_rx.borrow(),
    )?;
    let backend = backend(&state)?;
    backend
        .set_config(session_id.clone(), request.key, request.value)
        .await
        .map_err(|error| ApiFailure::conflict(format!("configuration failed: {error:#}")))?;
    let mut session = ApiSession::from(require_session_record(
        &state.snapshot_rx.borrow(),
        &session_id,
    )?);
    if let Some(handle) = backend.session_handle(session_id).await?
        && let Some(snapshot) = handle.view().snapshot
    {
        session.config_options = crate::server::session_config_view(
            session.harness_kind.parse()?,
            &snapshot.operational,
        );
    }
    Ok(Json(session))
}

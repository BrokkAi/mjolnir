//! The viewer's Settings page. These routes are on the signed-in viewer
//! surface only, never under `/api/v1`, whose bearer token agents inside
//! sessions hold: an agent must not rewrite the user's configuration.
//!
//! The browser holds the draft and sends it back with every request, so the
//! daemon keeps no per-browser state. Each page is rendered and each edit
//! applied by the settings model the terminal uses, `mj_client::settings`.
//! Literal environment values never reach the browser; see
//! `mj_client::settings::hidden`.
use super::*;
use mj_client::settings::{discovery, draft, hidden, save, view};
use mj_core::profile_capabilities::CapabilityState;
use mj_core::profile_capabilities::ProfileCapabilitiesSnapshot;
use mj_core::review::autoconfigure::autoconfigure;
use mj_core::worker_launch::ProfileConfig;
use serde_json::Value;

/// A whole draft and its original travel with every save.
pub(super) const MAX_SETTINGS_BODY_BYTES: usize = 1024 * 1024;

#[derive(Debug, Serialize)]
pub(super) struct SettingsDocument {
    original: Value,
    draft: Value,
    page: view::SettingsPage,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SettingsPageRequest {
    draft: Value,
    path: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct SettingsPageResponse {
    page: view::SettingsPage,
    /// The draft's discovered sub-agent choices, which the page refreshes
    /// while they are still being discovered.
    subagent_options: Value,
    reviewer_options: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SettingsEditRequest {
    draft: Value,
    /// The page the edit was made on.
    page: Vec<String>,
    edit: view::SettingsEdit,
}

/// A refused edit is an answer, not a failure: the draft comes back
/// unchanged with the reason, which the page shows beside the setting.
#[derive(Debug, Serialize)]
pub(super) struct SettingsEditResponse {
    draft: Value,
    page: view::SettingsPage,
    #[serde(skip_serializing_if = "Option::is_none")]
    notice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SettingsSaveRequest {
    original: Value,
    draft: Value,
    /// The page to show once saved, when it still exists.
    #[serde(default)]
    page: Vec<String>,
}

fn document(
    config: &Config,
    page: &[String],
    capabilities: &ProfileCapabilitiesSnapshot,
) -> SettingsDocument {
    let (mut original, mut draft) = draft::open(config);
    hidden::hide(&mut original);
    hidden::hide(&mut draft);
    // A saved configuration's selections are already hydrating.
    let unreachable = discover(&mut draft, capabilities, None);
    let page = render(&draft, page, unreachable.as_deref())
        .or_else(|_| render(&draft, &[], None))
        .expect("the first settings page always renders");
    SettingsDocument {
        original,
        draft,
        page,
    }
}

fn unreadable(error: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        format!("Settings could not read the configuration: {error}"),
    )
}

/// A draft is the stored shape of a configuration: an object.
fn check_draft(value: &Value) -> Result<(), ApiError> {
    if value.is_object() {
        Ok(())
    } else {
        Err(ApiError::bad_request("a settings draft must be an object"))
    }
}

pub(super) async fn read_settings(
    State(state): State<ServerState>,
) -> Result<Json<SettingsDocument>, ApiError> {
    let capabilities = capabilities(&state);
    tokio::task::spawn_blocking(move || {
        let config = Config::load().map_err(|error| unreadable(format!("{error:#}")))?;
        Ok(document(&config, &[], &capabilities))
    })
    .await
    .map_err(unreadable)?
    .map(Json)
}

/// Fill in what the daemon has discovered for `draft`: the sub-agent model
/// and effort choices. Their identity includes each profile's environment,
/// which the browser's draft carries hidden, so they are computed on a copy
/// with the stored values put back, and only the choices, which hold no
/// secrets, are copied over. A selected model's efforts are discovered once
/// it is selected, so `warm` is handed the draft. Returns what discovery
/// could not reach.
fn discover(
    draft: &mut Value,
    capabilities: &ProfileCapabilitiesSnapshot,
    warm: Option<&dyn api::SubagentBackend>,
) -> Option<String> {
    let discovered = (|| {
        let stored = serde_json::to_value(Config::load().ok()?).ok()?;
        let mut real = draft.clone();
        hidden::restore(&mut real, &stored).ok()?;
        if let Some(backend) = warm
            && let Ok(config) = draft::config_from_draft(real.clone())
        {
            backend.warm_profile_capabilities(&config);
        }
        let config = draft::config_from_draft(real.clone()).ok()?;
        let current = warm.and_then(|backend| backend.profile_capabilities_snapshot());
        let capabilities = current.as_ref().unwrap_or(capabilities);
        let action_reason = match capabilities.options(&config, None) {
            Some(options) if autoconfigure(&options.models, |_| None).is_some() => Value::Null,
            Some(options) if !options.unavailable.is_empty() => {
                Value::String(options.unavailable.join("; "))
            }
            Some(_) => Value::String("Autoconfigure unavailable: no supported models".into()),
            None => Value::String("Checking reviewer models…".into()),
        };
        let unreachable = discovery::fill_subagent_options(&mut real, capabilities).ok()?;
        discovery::fill_reviewer_options(&mut real, capabilities).ok()?;
        Some((
            real["_subagent_options"].take(),
            real["_reviewer_options"].take(),
            action_reason,
            unreachable,
        ))
    })();
    let (options, reviewer_options, action_reason, unreachable) = discovered.unwrap_or((
        Value::Null,
        Value::Null,
        Value::String("Reviewer discovery is unavailable.".into()),
        None,
    ));
    draft["_subagent_options"] = options;
    draft["_reviewer_options"] = reviewer_options;
    draft["_review_action_reason"] = action_reason;
    unreachable
}

/// Render `path` of a discovered draft, naming on the Sub-agents pages what
/// discovery could not reach.
fn render(
    draft: &Value,
    path: &[String],
    unreachable: Option<&str>,
) -> Result<view::SettingsPage, String> {
    let mut page = view::render_page(draft, path)?;
    if path == ["review"]
        && let Some(action) = page.actions.first_mut()
    {
        action.disabled_reason = draft["_review_action_reason"].as_str().map(str::to_owned);
    }
    if let Some(unreachable) = unreachable
        && path.first().is_some_and(|section| section == "subagents")
    {
        page.notices.push(unreachable.to_owned());
    }
    if path == ["review"] {
        let failures = draft["_reviewer_options"]
            .as_object()
            .into_iter()
            .flat_map(|options| options.values())
            .flat_map(|options| options["unavailable"].as_array().into_iter().flatten())
            .filter_map(Value::as_str)
            .filter(|reason| !reason.contains("discovery pending"))
            .collect::<std::collections::BTreeSet<_>>();
        if !failures.is_empty() {
            page.notices.push(format!(
                "Reviewer effort discovery failed: {}",
                failures.into_iter().collect::<Vec<_>>().join("; ")
            ));
        }
    }
    Ok(page)
}

fn capabilities(state: &ServerState) -> ProfileCapabilitiesSnapshot {
    state
        .snapshot_rx
        .borrow()
        .profile_capabilities
        .clone()
        .into_inner()
}

pub(super) async fn render_settings_page(
    State(state): State<ServerState>,
    Json(request): Json<SettingsPageRequest>,
) -> Result<Json<SettingsPageResponse>, ApiError> {
    check_draft(&request.draft)?;
    let capabilities = capabilities(&state);
    let backend = state.subagent.clone();
    // Discovery reads the configuration file.
    tokio::task::spawn_blocking(move || {
        let mut draft = request.draft;
        let unreachable = discover(&mut draft, &capabilities, backend.as_deref());
        let page =
            render(&draft, &request.path, unreachable.as_deref()).map_err(ApiError::not_found)?;
        Ok(SettingsPageResponse {
            page,
            subagent_options: draft["_subagent_options"].take(),
            reviewer_options: draft["_reviewer_options"].take(),
        })
    })
    .await
    .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
    .map(Json)
}

pub(super) async fn edit_settings(
    State(state): State<ServerState>,
    Json(request): Json<SettingsEditRequest>,
) -> Result<Json<SettingsEditResponse>, ApiError> {
    check_draft(&request.draft)?;
    let capabilities = capabilities(&state);
    let backend = state.subagent.clone();
    if let view::SettingsEdit::Action { id } = &request.edit {
        if id != "review.autoconfigure" || request.page != ["review"] {
            return Err(ApiError::bad_request("This action is unavailable here."));
        }
        return autoconfigure_settings(state, request).await.map(Json);
    }
    // Discovery reads the configuration file, and an edit may expand `~`
    // against this user's home directory.
    tokio::task::spawn_blocking(move || {
        let mut before = request.draft;
        let unreachable = discover(&mut before, &capabilities, None);
        let mut draft = before.clone();
        match view::apply(&mut draft, &request.page, request.edit) {
            Ok(result) => {
                // A new model offers its own efforts.
                let unreachable = discover(&mut draft, &capabilities, backend.as_deref());
                let page = render(&draft, &result.page, unreachable.as_deref())
                    .or_else(|_| render(&draft, &request.page, unreachable.as_deref()))
                    .map_err(ApiError::not_found)?;
                Ok(SettingsEditResponse {
                    draft,
                    page,
                    notice: result.notice,
                    error: None,
                })
            }
            Err(refusal) => {
                let page = render(&before, &request.page, unreachable.as_deref())
                    .map_err(ApiError::not_found)?;
                Ok(SettingsEditResponse {
                    draft: before,
                    page,
                    notice: None,
                    error: Some(refusal),
                })
            }
        }
    })
    .await
    .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
    .map(Json)
}

async fn autoconfigure_settings(
    state: ServerState,
    request: SettingsEditRequest,
) -> Result<SettingsEditResponse, ApiError> {
    let backend = state
        .subagent
        .clone()
        .ok_or_else(|| ApiError::bad_request("Reviewer discovery is unavailable."))?;
    let (mut draft, config, incoming) = tokio::task::spawn_blocking(move || {
        let incoming = request.draft.clone();
        let mut draft = request.draft;
        let stored =
            serde_json::to_value(Config::load().map_err(unreadable)?).map_err(unreadable)?;
        hidden::restore(&mut draft, &stored).map_err(ApiError::bad_request)?;
        let config = draft::config_from_draft(draft.clone())
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        Ok::<_, ApiError>((draft, config, incoming))
    })
    .await
    .map_err(unreadable)??;
    backend.warm_profile_capabilities(&config);
    let snapshot = backend
        .profile_capabilities_snapshot()
        .unwrap_or_else(|| capabilities(&state));
    let available = config
        .enabled_profiles()
        .filter_map(|(id, profile)| {
            let entry = snapshot.profiles.get(&profile.capabilities_key(id))?;
            let CapabilityState::Ready(choices) = &entry.choices else {
                return None;
            };
            Some((
                id.to_owned(),
                ProfileConfig {
                    model: choices.model.clone(),
                    models: choices.models.clone(),
                    efforts: choices.efforts.clone(),
                    observed_at: 0,
                },
            ))
        })
        .collect::<Vec<_>>();
    let profile_keys = config
        .enabled_profiles()
        .map(|(id, profile)| (id.to_owned(), profile.capabilities_key(id)))
        .collect::<std::collections::BTreeMap<_, _>>();
    let (configured, known) = crate::review_selection::autoconfigure_with_efforts(
        &available,
        |profile, model| {
            let backend = backend.clone();
            let mut demand = config.clone();
            demand.review.reviewers = vec![mj_core::config::SubagentChoice {
                model: model.clone(),
                effort: None,
            }];
            demand.review.clear_legacy();
            backend.warm_profile_capabilities(&demand);
            let initial_snapshot = snapshot.clone();
            let key = profile_keys.get(&profile).cloned();
            async move {
                let key =
                    key.ok_or_else(|| anyhow::anyhow!("draft profile {profile:?} disappeared"))?;
                loop {
                    let latest = backend.profile_capabilities_snapshot();
                    if let Some(entry) = latest
                        .as_ref()
                        .and_then(|snapshot| snapshot.profiles.get(&key))
                        .or_else(|| initial_snapshot.profiles.get(&key))
                    {
                        match entry.efforts.get(&model) {
                            Some(CapabilityState::Ready(efforts)) => {
                                let CapabilityState::Ready(mut choices) = entry.choices.clone()
                                else {
                                    anyhow::bail!(
                                        "draft profile {profile:?} discovery is unavailable"
                                    )
                                };
                                choices.efforts = efforts.clone();
                                return Ok(choices);
                            }
                            Some(CapabilityState::Failed(error)) => anyhow::bail!("{error}"),
                            _ => {}
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        },
        None,
    )
    .await
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let mut reviewers = configured.reviewers;
    if reviewers.is_empty() {
        return Err(ApiError::bad_request(
            "Autoconfigure unavailable: no supported models",
        ));
    }
    let unknown = known
        .iter()
        .filter_map(|(model, known)| (!known).then_some(model.clone()))
        .collect::<Vec<_>>();
    for reviewer in &mut reviewers {
        if unknown.contains(&reviewer.model) {
            reviewer.effort = None;
        }
    }
    let mut notices = Vec::new();
    if !unknown.is_empty() {
        notices.push(format!("Could not confirm reviewer efforts for {} (discovery failed or timed out); using defaults.", unknown.join(", ")));
    }
    if !configured.adjusted.is_empty() {
        notices.push(format!(
            "Adjusted reviewer efforts: {}",
            configured
                .adjusted
                .iter()
                .map(|item| format!(
                    "{}: {} → {}",
                    item.model,
                    item.requested,
                    item.used.as_deref().unwrap_or("default")
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let notice = (!notices.is_empty()).then(|| notices.join(" "));
    draft["review"]["reviewers"] = serde_json::to_value(reviewers).map_err(unreadable)?;
    for key in ["count", "profile", "effort"] {
        draft["review"].as_object_mut().unwrap().remove(key);
    }
    hidden::rehide_restored(&mut draft, &incoming);
    let snapshot = capabilities(&state);
    let backend = state.subagent.clone();
    tokio::task::spawn_blocking(move || {
        discover(&mut draft, &snapshot, backend.as_deref());
        let page = render(&draft, &["review".into()], None).map_err(ApiError::not_found)?;
        Ok(SettingsEditResponse {
            draft,
            page,
            notice,
            error: None,
        })
    })
    .await
    .map_err(unreadable)?
}

pub(super) async fn save_settings(
    State(state): State<ServerState>,
    Json(request): Json<SettingsSaveRequest>,
) -> Response<Body> {
    if let Err(error) = check_draft(&request.original).and(check_draft(&request.draft)) {
        return error.into_response();
    }
    // A save is short daemon-owned work. While an upgrade handoff waits it
    // is refused exactly as the closed gate refuses a request, before
    // anything is written, so the viewer keeps the draft and sends the same
    // save to the next daemon.
    let Ok(_admission) = state.upgrade_gate.enter_unless_draining("settings save") else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("retry-after", "1"), (UPGRADE_HEADER, "pending")],
            "Mjolnir is completing an upgrade",
        )
            .into_response();
    };
    match save_draft_settings(request, capabilities(&state)).await {
        Ok(document) => Json(document).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn save_draft_settings(
    request: SettingsSaveRequest,
    capabilities: ProfileCapabilitiesSnapshot,
) -> Result<SettingsDocument, ApiError> {
    let SettingsSaveRequest {
        mut original,
        draft,
        page,
    } = request;
    tokio::task::spawn_blocking(move || {
        let current = Config::load().map_err(|error| unreadable(format!("{error:#}")))?;
        let stored = serde_json::to_value(&current).map_err(unreadable)?;
        let mut draft = draft;
        let mut secrets = hidden::restore(&mut original, &stored).map_err(ApiError::bad_request)?;
        secrets.extend(hidden::restore(&mut draft, &stored).map_err(ApiError::bad_request)?);
        let refused = |error: anyhow::Error| {
            let status = if error.downcast_ref::<save::EditConflict>().is_some() {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            ApiError::new(status, hidden::scrub(&format!("{error:#}"), &secrets))
        };
        let updated =
            draft::config_from_draft(draft).map_err(|error| refused(anyhow::Error::new(error)))?;
        if !updated.review.is_legacy()
            && updated.review.trigger != mj_core::config::ReviewTrigger::Off
            && updated.review.reviewers.is_empty()
        {
            return Err(ApiError::bad_request(
                "Add reviewer on Settings › Code Review before enabling automatic review.",
            ));
        }
        let updated = serde_json::to_string(&updated).map_err(|error| refused(error.into()))?;
        let sessions = crate::database::load_state().map_err(|error| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Settings could not read session state: {error:#}"),
            )
        })?;
        let saved = save::save_at(
            &mj_core::config::config_path(),
            &original.to_string(),
            &updated,
            &sessions,
        )
        .map_err(refused)?;
        Ok(document(&saved.with_local_targets(), &page, &capabilities))
    })
    .await
    .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
}

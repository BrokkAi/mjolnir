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
use mj_core::profile_capabilities::ProfileCapabilitiesSnapshot;
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
    let unreachable = discover(&mut draft, capabilities);
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
/// secrets, are copied over. Returns what discovery could not reach.
fn discover(draft: &mut Value, capabilities: &ProfileCapabilitiesSnapshot) -> Option<String> {
    let discovered = (|| {
        let stored = serde_json::to_value(Config::load().ok()?).ok()?;
        let mut real = draft.clone();
        hidden::restore(&mut real, &stored).ok()?;
        let unreachable = discovery::fill_subagent_options(&mut real, capabilities).ok()?;
        Some((real["_subagent_options"].take(), unreachable))
    })();
    let (options, unreachable) = discovered.unwrap_or((Value::Null, None));
    draft["_subagent_options"] = options;
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
    if let Some(unreachable) = unreachable
        && path.first().is_some_and(|section| section == "subagents")
    {
        page.notices.push(unreachable.to_owned());
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
    // Discovery reads the configuration file.
    tokio::task::spawn_blocking(move || {
        let mut draft = request.draft;
        let unreachable = discover(&mut draft, &capabilities);
        let page =
            render(&draft, &request.path, unreachable.as_deref()).map_err(ApiError::not_found)?;
        Ok(SettingsPageResponse { page })
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
    // Discovery reads the configuration file, and an edit may expand `~`
    // against this user's home directory.
    tokio::task::spawn_blocking(move || {
        let mut before = request.draft;
        let unreachable = discover(&mut before, &capabilities);
        let mut draft = before.clone();
        match view::apply(&mut draft, &request.page, request.edit) {
            Ok(result) => {
                // A new model offers its own efforts.
                let unreachable = discover(&mut draft, &capabilities);
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

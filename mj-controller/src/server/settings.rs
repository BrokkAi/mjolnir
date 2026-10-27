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
use mj_client::settings::{draft, hidden, save, view};
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

fn document(config: &Config, page: &[String]) -> SettingsDocument {
    let (mut original, mut draft) = draft::open(config);
    hidden::hide(&mut original);
    hidden::hide(&mut draft);
    let page = view::render_page(&draft, page)
        .or_else(|_| view::render_page(&draft, &[]))
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
    State(_): State<ServerState>,
) -> Result<Json<SettingsDocument>, ApiError> {
    let config = tokio::task::spawn_blocking(Config::load)
        .await
        .map_err(unreadable)?
        .map_err(|error| unreadable(format!("{error:#}")))?;
    Ok(Json(document(&config, &[])))
}

pub(super) async fn render_settings_page(
    State(_): State<ServerState>,
    Json(request): Json<SettingsPageRequest>,
) -> Result<Json<SettingsPageResponse>, ApiError> {
    check_draft(&request.draft)?;
    let page = view::render_page(&request.draft, &request.path).map_err(ApiError::not_found)?;
    Ok(Json(SettingsPageResponse { page }))
}

pub(super) async fn edit_settings(
    State(_): State<ServerState>,
    Json(request): Json<SettingsEditRequest>,
) -> Result<Json<SettingsEditResponse>, ApiError> {
    check_draft(&request.draft)?;
    // An edit may expand `~` against this user's home directory, which reads
    // the environment and the password database.
    tokio::task::spawn_blocking(move || {
        let mut draft = request.draft.clone();
        match view::apply(&mut draft, &request.page, request.edit) {
            Ok(result) => {
                let page = view::render_page(&draft, &result.page)
                    .or_else(|_| view::render_page(&draft, &request.page))
                    .map_err(ApiError::not_found)?;
                Ok(SettingsEditResponse {
                    draft,
                    page,
                    notice: result.notice,
                    error: None,
                })
            }
            Err(refusal) => {
                let page = view::render_page(&request.draft, &request.page)
                    .map_err(ApiError::not_found)?;
                Ok(SettingsEditResponse {
                    draft: request.draft,
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
    match save_draft_settings(request).await {
        Ok(document) => Json(document).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn save_draft_settings(request: SettingsSaveRequest) -> Result<SettingsDocument, ApiError> {
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
        Ok(document(&saved.with_local_targets(), &page))
    })
    .await
    .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
}

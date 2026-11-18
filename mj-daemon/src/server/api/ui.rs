//! `POST /ui/requests`: change what the person's dashboard shows (`mj ui`).
//!
//! The request is checked and published to the attached dashboards, which
//! apply it as input. Settings are only staged; the person saves them, so a
//! bearer-token caller still cannot write the configuration.

use super::*;
use mj_client::ui_request::{UiRequestBody, UiRequestReceipt, UiRequestRefusal};

pub(super) async fn request_ui(
    State(state): State<ServerState>,
    Json(body): Json<UiRequestBody>,
) -> Result<Json<UiRequestReceipt>, ApiFailure> {
    match backend(&state)?.request_ui(body) {
        Ok(receipt) => Ok(Json(receipt)),
        Err(refusal @ UiRequestRefusal::Invalid(_)) => {
            Err(ApiFailure::bad_request(refusal.to_string()))
        }
        Err(refusal @ UiRequestRefusal::NoDashboard) => {
            Err(ApiFailure::conflict(refusal.to_string()))
        }
    }
}

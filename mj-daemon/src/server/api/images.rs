use super::*;
use mj_client::session_image::{ImageFollow, ImageRequest, ImageStarted};

pub(super) async fn start(
    State(state): State<ServerState>,
    Json(request): Json<ImageRequest>,
) -> Result<Json<ImageStarted>, ApiError> {
    state
        .image_builds
        .start(request, false, &state.shutdown)
        .map(Json)
        .map_err(|error| ApiError::bad_request(format!("{error:#}")))
}

pub(super) async fn status(
    State(state): State<ServerState>,
    Json(request): Json<ImageRequest>,
) -> Result<Json<ImageStarted>, ApiError> {
    state
        .image_builds
        .start(request, true, &state.shutdown)
        .map(Json)
        .map_err(|error| ApiError::bad_request(format!("{error:#}")))
}

#[derive(Deserialize)]
pub(super) struct FollowQuery {
    #[serde(default)]
    offset: usize,
}

pub(super) async fn follow(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Query(query): Query<FollowQuery>,
) -> Result<Json<ImageFollow>, ApiFailure> {
    let result = tokio::select! {
        biased;
        _ = state.shutdown.cancelled() => return Err(ApiFailure::handoff()),
        result = state.image_builds.follow(&id, query.offset) => result,
    };
    if state.shutdown.is_cancelled() {
        return Err(ApiFailure::handoff());
    }
    result
        .map_err(|error| ApiFailure::bad_request(format!("{error:#}")))?
        .map(Json)
        .ok_or_else(|| {
            ApiFailure::not_found("unknown image operation; its daemon may have been replaced")
        })
}

pub(super) async fn cancel(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    if state.image_builds.cancel(&id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown image operation",
        ))
    }
}

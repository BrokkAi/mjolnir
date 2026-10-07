use super::*;

const MAX_EVENT_KEY_BYTES: usize = 512;
const MAX_EVENT_TEXT_BYTES: usize = 64 * 1024;

pub(super) async fn enqueue_event(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<MailboxEventRequest>,
) -> Result<(StatusCode, Json<MailboxEventResponse>), ApiFailure> {
    require_session_record(&state.snapshot_rx.borrow(), &session_id)?;
    if request.key.trim().is_empty() || request.key.len() > MAX_EVENT_KEY_BYTES {
        return Err(ApiFailure::bad_request(format!(
            "event key must contain 1 to {MAX_EVENT_KEY_BYTES} bytes"
        )));
    }
    if request.text.trim().is_empty() || request.text.len() > MAX_EVENT_TEXT_BYTES {
        return Err(ApiFailure::bad_request(format!(
            "event text must contain 1 to {MAX_EVENT_TEXT_BYTES} bytes"
        )));
    }
    let MailboxEventRequest { key, text, wake } = request;
    let event_key = format!("api:{session_id}:{key}");
    let created_at_ms = mj_core::clock::epoch_millis().max(0) as u64;
    let event_json = serde_json::to_string(&serde_json::json!({
        "key": event_key.clone(),
        "source": "api",
        "wake": wake,
        "text": text,
        "created_at_ms": created_at_ms,
    }))
    .map_err(anyhow::Error::from)?;
    let target = session_id.clone();
    let inserted = tokio::task::spawn_blocking(move || {
        crate::database::enqueue_mailbox_event(&event_key, &target, &event_json, wake, false)
    })
    .await
    .map_err(|error| anyhow::anyhow!("mailbox outbox write task failed: {error}"))??;
    Ok((
        StatusCode::ACCEPTED,
        Json(MailboxEventResponse { key, inserted }),
    ))
}

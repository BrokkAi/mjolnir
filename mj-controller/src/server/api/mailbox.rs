use super::*;

const MAX_EVENT_KEY_BYTES: usize = 512;
const MAX_EVENT_TEXT_BYTES: usize = 64 * 1024;

pub(super) async fn enqueue_event(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<MailboxEventRequest>,
) -> Result<(StatusCode, Json<MailboxEventResponse>), ApiFailure> {
    let mailboxes_enabled = {
        let snapshot = state.snapshot_rx.borrow();
        require_session_record(&snapshot, &session_id)?;
        snapshot.agent_mailboxes_enabled
    };
    if !mailboxes_enabled {
        return Err(ApiFailure::conflict(
            "agent mailboxes are disabled; enable Agent mailboxes and Jev in Settings",
        ));
    }
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
    let event = mj_core::mailbox::MailboxEvent {
        key: event_key.clone(),
        source: "api".into(),
        wake,
        created_at_ms,
        body: mj_core::mailbox::MailboxEventBody::PlainText { text },
    };
    let event_json = serde_json::to_string(&event).map_err(anyhow::Error::from)?;
    let target = session_id.clone();
    let admission = state
        .upgrade_gate
        .enter("API mailbox event")
        .map_err(|_| ApiFailure::shutdown(&state))?;
    let blocking_admission = admission.clone();
    let inserted = tokio::task::spawn_blocking(move || {
        let _admission = blocking_admission;
        crate::database::enqueue_mailbox_event(&event_key, &target, &event_json, wake, false)
    })
    .await
    .map_err(|error| anyhow::anyhow!("mailbox outbox write task failed: {error}"))??;
    Ok((
        StatusCode::ACCEPTED,
        Json(MailboxEventResponse { key, inserted }),
    ))
}

pub(super) async fn send_message(
    State(state): State<ServerState>,
    Path(session_id): Path<String>,
    Json(request): Json<SessionMessageRequest>,
) -> Result<(StatusCode, Json<SessionMessageResponse>), ApiFailure> {
    super::super::validate_public_id(&request.request_id)?;
    if request.text.trim().is_empty() || request.text.len() > MAX_EVENT_TEXT_BYTES {
        return Err(ApiFailure::bad_request(format!(
            "message text must contain 1 to {MAX_EVENT_TEXT_BYTES} bytes"
        )));
    }
    let response = backend(&state)?
        .clone()
        .deliver_message(
            request.sender_session_id,
            session_id,
            request.text,
            request.request_id,
            mj_core::clock::epoch_millis(),
        )
        .await?;
    Ok((StatusCode::ACCEPTED, Json(response)))
}

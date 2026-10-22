use super::*;
use crate::database::{ApiEvent, ApiEventData, ApiEventFilter, ApiEventPage};
use axum::http::HeaderMap;
use axum::response::Sse;
use axum::response::sse::{Event, KeepAlive};
use tokio_stream::wrappers::ReceiverStream;

#[derive(Debug, Default, Deserialize)]
pub(super) struct EventsQuery {
    session_id: Option<String>,
    workspace_id: Option<String>,
    after_seq: Option<u64>,
}

pub(super) async fn events(
    State(state): State<ServerState>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiFailure> {
    let header_cursor = headers
        .get("last-event-id")
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| ApiFailure::bad_request("Last-Event-ID must be an unsigned integer"))
        })
        .transpose()?;
    if let (Some(query), Some(header)) = (query.after_seq, header_cursor)
        && query != header
    {
        return Err(ApiFailure::bad_request(
            "after_seq and Last-Event-ID disagree",
        ));
    }
    let filter = ApiEventFilter {
        session_id: query.session_id,
        workspace_id: query.workspace_id,
    };
    if let Some(session_id) = filter.session_id.as_deref() {
        require_session_record(&state.snapshot_rx.borrow(), session_id)?;
    }
    let after_seq = query.after_seq.or(header_cursor);
    if after_seq.is_some_and(|cursor| cursor > i64::MAX as u64) {
        return Err(ApiFailure::bad_request(
            "event cursor exceeds the supported range",
        ));
    }
    // Subscribe before reading history. Catch-up then reads the latest activity
    // snapshot, so changes during that read cannot fall between two owners.
    let mut snapshots = state.snapshot_rx.clone();
    let backend = backend(&state)?.clone();
    let first = backend.events(filter.clone(), after_seq).await?;
    if after_seq.is_some_and(|cursor| cursor > first.latest_seq) {
        return Err(ApiFailure::bad_request(
            "event cursor is ahead of this database",
        ));
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(32);
    let shutdown = state.shutdown.clone();
    let stream_state = state.clone();
    tokio::spawn(async move {
        // The cursor through which events are queued to this client. A page
        // read without a cursor starts at the frontier it read.
        let mut delivered = after_seq.unwrap_or(first.latest_seq);
        let mut page = first;
        let mut previous_activity = super::super::ViewerSessions::default();
        let mut snapshots_open = true;
        let mut poll = tokio::time::interval(Duration::from_millis(250));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let stopped = 'stream: loop {
            let next = page.next_after_seq;
            let caught_up = next >= page.latest_seq;
            for event in page.events.drain(..) {
                let seq = event.seq;
                let encoded = Event::default()
                    .id(seq.to_string())
                    .event(event.event.kind())
                    .json_data(&event);
                match encoded {
                    Ok(event) => {
                        tokio::select! {
                            () = shutdown.cancelled() => break,
                            sent = tx.send(Ok(event)) => if sent.is_err() { return; },
                        }
                        delivered = seq;
                    }
                    Err(error) => {
                        tracing::error!(%error, "encode native API event");
                        return;
                    }
                }
            }
            if shutdown.is_cancelled() {
                break Stop::Shutdown;
            }
            delivered = delivered.max(next);
            if caught_up {
                let current = snapshots.borrow_and_update().sessions.clone();
                let activity = previous_activity
                    .0
                    .changes(&current.0)
                    .filter_map(|(id, row)| {
                        let row = row.filter(|row| matches_activity(row, &filter))?;
                        let activity = row.api_activity();
                        let previous = previous_activity
                            .0
                            .get(id)
                            .filter(|row| matches_activity(row, &filter))
                            .map(|row| row.api_activity());
                        (previous.as_ref() != Some(&activity)).then(|| ApiEvent {
                            // A transient frame's seq is its durable resume position,
                            // not a new identity. Only durable frames receive SSE IDs.
                            seq: delivered,
                            session_id: id.clone(),
                            recorded_at_ms: chrono::Utc::now().timestamp_millis(),
                            event: ApiEventData::ActivityChanged { activity },
                        })
                    })
                    .collect::<Vec<_>>();
                for event in activity {
                    let encoded = match Event::default().event(event.event.kind()).json_data(&event)
                    {
                        Ok(event) => event,
                        Err(error) => {
                            tracing::error!(%error, "encode transient API activity");
                            break 'stream Stop::Failed;
                        }
                    };
                    tokio::select! {
                        () = shutdown.cancelled() => break 'stream Stop::Shutdown,
                        sent = tx.send(Ok(encoded)) => if sent.is_err() { return; },
                    }
                }
                previous_activity = current;
                tokio::select! {
                    () = shutdown.cancelled() => break Stop::Shutdown,
                    () = tx.closed() => return,
                    changed = snapshots.changed(), if snapshots_open => {
                        snapshots_open = changed.is_ok();
                        // Activity does not poll SQLite. Keep the independent
                        // durable tick so a busy snapshot feed cannot starve it.
                        continue;
                    }
                    _ = poll.tick() => {}
                }
            }
            let result = tokio::select! {
                () = shutdown.cancelled() => break Stop::Shutdown,
                () = tx.closed() => return,
                page = backend.events(filter.clone(), Some(next)) => page,
            };
            match result {
                Ok(next_page) => page = next_page,
                Err(error) => {
                    tracing::warn!(%error, "read native API event stream");
                    break Stop::Failed;
                }
            }
        };
        match stopped {
            // The daemon is being replaced: name the cursor to resume from,
            // so the client follows the stream onto the next daemon without
            // losing or repeating an event. Shutdown never waits on a reader
            // that stopped reading, so this is sent only if it fits. A read
            // that failed during the handoff is part of the same teardown.
            Stop::Shutdown | Stop::Failed if stream_state.handing_off() => {
                let _ = tx.try_send(Ok(Event::default()
                    .event(DAEMON_HANDOFF_CODE)
                    .id(delivered.to_string())
                    .data("the daemon is being replaced; reconnect from the last event ID")));
            }
            Stop::Shutdown => {}
            Stop::Failed => {
                tokio::select! {
                    () = shutdown.cancelled() => {},
                    _ = tx.send(Ok(Event::default().event("stream_error").data("event stream unavailable; reconnect from the last event ID"))) => {},
                }
            }
        }
    });
    Ok(Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
        .into_response())
}

fn matches_activity(session: &ViewerSession, filter: &ApiEventFilter) -> bool {
    filter
        .session_id
        .as_ref()
        .is_none_or(|id| id == &session.id)
        && filter
            .workspace_id
            .as_ref()
            .is_none_or(|id| id == &session.workspace_id)
}

/// Why an event stream stopped before its client went away.
enum Stop {
    Shutdown,
    Failed,
}

/// The default backend reads bounded durable pages away from the async runtime.
pub(super) fn load_events(
    filter: ApiEventFilter,
    after_seq: Option<u64>,
) -> BoxFuture<'static, AnyResult<ApiEventPage>> {
    Box::pin(async move {
        tokio::task::spawn_blocking(move || {
            crate::database::load_api_events(&filter, after_seq, 200)
        })
        .await?
    })
}

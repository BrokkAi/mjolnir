use super::*;
use axum::http::HeaderMap;
use axum::response::Sse;
use axum::response::sse::{Event, KeepAlive};
use hel::hel_database::{ApiEventFilter, ApiEventPage};
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
    let after_seq = query.after_seq.or(header_cursor);
    if after_seq.is_some_and(|cursor| cursor > i64::MAX as u64) {
        return Err(ApiFailure::bad_request(
            "event cursor exceeds the supported range",
        ));
    }
    let backend = backend(&state)?.clone();
    let first = backend.events(filter.clone(), after_seq).await?;
    if after_seq.is_some_and(|cursor| cursor > first.latest_seq) {
        return Err(ApiFailure::bad_request(
            "event cursor is ahead of this database",
        ));
    }
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(32);
    let shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        let mut page = first;
        loop {
            let next = page.next_after_seq;
            let caught_up = next >= page.latest_seq;
            for event in page.events {
                let encoded = Event::default()
                    .id(event.seq.to_string())
                    .event(event.event.kind())
                    .json_data(&event);
                match encoded {
                    Ok(event) => {
                        tokio::select! {
                            () = shutdown.cancelled() => return,
                            sent = tx.send(Ok(event)) => if sent.is_err() { return; },
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "encode native API event");
                        return;
                    }
                }
            }
            if caught_up {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tx.closed() => return,
                    () = tokio::time::sleep(Duration::from_millis(250)) => {}
                }
            }
            let result = tokio::select! {
                () = shutdown.cancelled() => return,
                () = tx.closed() => return,
                page = backend.events(filter.clone(), Some(next)) => page,
            };
            match result {
                Ok(next_page) => page = next_page,
                Err(error) => {
                    tracing::warn!(%error, "read native API event stream");
                    tokio::select! {
                        () = shutdown.cancelled() => {},
                        _ = tx.send(Ok(Event::default().event("stream_error").data("event stream unavailable; reconnect from the last event ID"))) => {},
                    }
                    return;
                }
            }
        }
    });
    Ok(Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// The default backend reads bounded durable pages away from the async runtime.
pub(super) fn load_events(
    filter: ApiEventFilter,
    after_seq: Option<u64>,
) -> BoxFuture<'static, AnyResult<ApiEventPage>> {
    Box::pin(async move {
        tokio::task::spawn_blocking(move || {
            hel::hel_database::load_api_events(&filter, after_seq, 200)
        })
        .await?
    })
}

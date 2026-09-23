//! Record the UI's activity facts without doing database work on its loop,
//! and without letting a recording failure stop the server.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use crate::database::ApiActivityState;
use crate::server::ViewerSnapshot;
use anyhow::{Context, Result};

/// Drain activity changes into the database for as long as the viewer
/// publishes them.
///
/// A recording failure is not fatal: `recorded` is left where it was so the
/// same diff is retried on the next snapshot, and the loop keeps running. The
/// only way out is the watch sender going away.
pub(super) async fn record_activity_stream(
    mut snapshots: tokio::sync::watch::Receiver<ViewerSnapshot>,
    record: impl FnMut(Vec<(String, ApiActivityState)>, i64) -> Result<()> + Send + 'static,
) -> Result<()> {
    let record = Arc::new(Mutex::new(record));
    let mut recorded = BTreeMap::<String, ApiActivityState>::new();
    let mut last_failure: Option<String> = None;
    loop {
        let current = {
            let snapshot = snapshots.borrow_and_update();
            snapshot
                .sessions
                .iter()
                .map(|session| {
                    (
                        session.id.clone(),
                        ApiActivityState {
                            state: session.state.clone(),
                            details: session.activity_details.clone(),
                            is_idle: session.is_idle,
                            waiting_for_input: !session.pending_elicitations.is_empty(),
                            capacity_retry: session.capacity_retry.is_some(),
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        let changed = current
            .iter()
            .filter(|(id, activity)| recorded.get(*id) != Some(*activity))
            .map(|(id, activity)| (id.clone(), activity.clone()))
            .collect::<Vec<_>>();
        if changed.is_empty() {
            recorded = current;
        } else {
            let record = Arc::clone(&record);
            let outcome = tokio::task::spawn_blocking(move || {
                let mut record = record.lock().unwrap_or_else(PoisonError::into_inner);
                record(changed, chrono::Utc::now().timestamp_millis())
            })
            .await
            .context("API activity recorder task failed");
            match outcome.and_then(|result| result) {
                Ok(()) => {
                    if last_failure.take().is_some() {
                        tracing::info!("API activity recorder recovered");
                    }
                    recorded = current;
                }
                Err(error) => {
                    // Keep `recorded` where it was: the same diff is retried on
                    // the next snapshot. Log only when the failure is new, so a
                    // persistent one (a stopped writer during shutdown) cannot
                    // flood the log.
                    let text = format!("{error:#}");
                    if last_failure.as_deref() == Some(text.as_str()) {
                        tracing::debug!(%error, "the native API activity recorder is still failing");
                    } else if crate::database::is_busy_error(&error) {
                        tracing::warn!(
                            %error,
                            "the native API activity recorder found the database busy; retrying on the next change"
                        );
                    } else {
                        tracing::warn!(
                            %error,
                            "the native API activity recorder could not record a change; retrying on the next change"
                        );
                    }
                    last_failure = Some(text);
                }
            }
        }
        if snapshots.changed().await.is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ViewerSession` has no `Default`, and only its id and state matter
    /// here, so the fixture deserializes the required fields.
    fn snapshot(sessions: &[(&str, &str)]) -> ViewerSnapshot {
        ViewerSnapshot {
            sessions: sessions
                .iter()
                .map(|(id, state)| {
                    serde_json::from_value(serde_json::json!({
                        "id": id,
                        "title": id,
                        "harness_kind": "claude",
                        "profile_id": "profile-1",
                        "bundle_id": "bundle-1",
                        "target_id": "target-1",
                        "state": state,
                        "created_at": "2026-01-01T00:00:00Z",
                        "updated_at": "2026-01-01T00:00:00Z",
                        "has_error": false,
                        "conversation_available": false,
                        "lifecycle": "live",
                        "capabilities":
                            serde_json::to_value(crate::server::ViewerSessionCapabilities::default())
                                .unwrap(),
                    }))
                    .unwrap()
                })
                .collect(),
            ..ViewerSnapshot::default()
        }
    }

    fn busy() -> anyhow::Error {
        anyhow::Error::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(5),
            Some("database is locked".into()),
        ))
        .context("record_api_activities")
    }

    #[tokio::test]
    async fn recorder_retries_the_same_diff_after_a_failure_and_keeps_running() {
        let (sender, receiver) = tokio::sync::watch::channel(snapshot(&[]));
        // A tokio channel: awaiting keeps the test's runtime free to poll the
        // recorder, which a blocking `std::sync::mpsc` recv would starve.
        let (calls_tx, mut calls) = tokio::sync::mpsc::unbounded_channel();
        let mut attempt = 0_usize;
        let recorder = tokio::spawn(record_activity_stream(receiver, move |changed, _at| {
            attempt += 1;
            calls_tx.send(changed).unwrap();
            if attempt == 1 { Err(busy()) } else { Ok(()) }
        }));

        sender.send(snapshot(&[("session-1", "working")])).unwrap();
        let first = calls.recv().await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, "session-1");

        // The failed diff was not retired, so the next attempt carries
        // session-1 again alongside the new session.
        sender
            .send(snapshot(&[("session-1", "working"), ("session-2", "idle")]))
            .unwrap();
        let second = calls.recv().await.unwrap();
        let ids = second.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, ["session-1", "session-2"]);

        // The loop is still alive after the failure.
        sender
            .send(snapshot(&[("session-1", "done"), ("session-2", "idle")]))
            .unwrap();
        let third = calls.recv().await.unwrap();
        assert_eq!(
            third.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            ["session-1"]
        );

        drop(sender);
        recorder.await.unwrap().unwrap();
    }
}

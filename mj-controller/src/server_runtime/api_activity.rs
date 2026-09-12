//! Record the UI's activity facts without doing database work on its loop.
use std::collections::BTreeMap;

use crate::database::ApiActivityState;
use crate::server::ViewerSnapshot;
use anyhow::{Context, Result};

pub(super) async fn record_activity_stream(
    mut snapshots: tokio::sync::watch::Receiver<ViewerSnapshot>,
) -> Result<()> {
    let mut recorded = BTreeMap::<String, ApiActivityState>::new();
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
        if !changed.is_empty() {
            tokio::task::spawn_blocking(move || {
                crate::database::record_api_activities(
                    changed,
                    chrono::Utc::now().timestamp_millis(),
                )
            })
            .await
            .context("API activity recorder task failed")??;
        }
        recorded = current;
        if snapshots.changed().await.is_err() {
            return Ok(());
        }
    }
}

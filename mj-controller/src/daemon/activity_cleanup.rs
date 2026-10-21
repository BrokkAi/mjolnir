//! Disposable API history is reclaimed beside serving, one bounded write at a time.
use anyhow::{Context, Result};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub(super) async fn run(cancellation: CancellationToken) -> Result<()> {
    let mut after_seq = 0;
    let mut removed = 0;
    let mut reported = Instant::now();
    tracing::info!("scanning historical API activity for cleanup");
    loop {
        if cancellation.is_cancelled() {
            return Ok(());
        }
        // Only the bounded writer job holds admission. The next daemon can
        // safely restart the scan, so draining refuses the next batch.
        let Ok(work) = crate::upgrade::activity_unless_draining("API activity history cleanup")
        else {
            return Ok(());
        };
        let batch = tokio::task::spawn_blocking(move || {
            let _work = work;
            crate::database::prune_api_activity_events(after_seq)
        })
        .await
        .context("API activity cleanup task failed")??;
        let Some((through, deleted)) = batch else {
            tracing::info!(removed, "historical API activity cleanup finished");
            return Ok(());
        };
        after_seq = through;
        removed += deleted;
        if reported.elapsed() >= Duration::from_secs(5) {
            tracing::info!(
                removed,
                through,
                "historical API activity cleanup in progress"
            );
            reported = Instant::now();
        }
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            () = tokio::time::sleep(Duration::from_millis(25)) => {}
        }
    }
}

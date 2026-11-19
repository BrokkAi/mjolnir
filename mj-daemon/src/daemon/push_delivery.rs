//! The daemon's Web Push delivery loop.
use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};

use crate::daemon::RuntimeState;
use mj_client::runtime_feed::RuntimeProjection;
use mj_controller::database as db;
use mj_controller::web_push::*;
use std::collections::BTreeMap;

type NotificationInputs = Option<BTreeMap<String, PushEntry<String>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotificationObservation {
    Unchanged,
    Deferred,
    /// The write failed; the inputs stay unrecorded so the next revision retries.
    Failed,
    Observed(bool),
}

/// Remembers only notification inputs, never transcript history or delivery state.
#[derive(Default)]
pub(crate) struct NotificationReconciler {
    previous: Option<NotificationInputs>,
}

impl NotificationReconciler {
    pub(crate) async fn reconcile(
        &mut self,
        runtime: RuntimeProjection,
    ) -> Result<NotificationObservation> {
        let inputs = card_entries(&runtime).map(|entries| {
            entries
                .into_iter()
                .map(|(identity, entry)| (identity, entry.owned()))
                .collect()
        });
        if self.previous.as_ref() == Some(&inputs) {
            return Ok(NotificationObservation::Unchanged);
        }
        // Only a panicked storage task is fatal; a failed write is retried.
        let result = blocking(move || Ok(db::notification_reconcile(runtime))).await?;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(
                    error = format!("{error:#}"),
                    "notification reconciliation failed; the next runtime revision retries"
                );
                return Ok(NotificationObservation::Failed);
            }
        };
        // Refused observations were not accepted: retain the previous inputs so
        // a lapsed handoff can reobserve them, as a fresh daemon would.
        match result {
            Some(result) => {
                // Partial observations commit successful items, but the next
                // runtime revision must retry any unreadable items.
                self.previous = result.complete.then_some(inputs);
                Ok(NotificationObservation::Observed(result.alerts_changed))
            }
            None => Ok(NotificationObservation::Deferred),
        }
    }
}

/// A refused observation waits on the admission owner, not an unrelated runtime event.
pub(crate) async fn wait_for_notification_change(
    observation: NotificationObservation,
    revisions: &mut tokio::sync::watch::Receiver<u64>,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<bool> {
    if observation == NotificationObservation::Deferred {
        tokio::select! {
            _ = cancellation.cancelled() => Ok(false),
            resumed = mj_controller::upgrade::gate().wait_after_draining() => {
                if resumed { Ok(true) } else { cancellation.cancelled().await; Ok(false) }
            }
        }
    } else {
        tokio::select! {
            _ = cancellation.cancelled() => Ok(false),
            changed = revisions.changed() => { changed.context("notification runtime feed closed")?; Ok(true) },
        }
    }
}

/// Durable notify decisions belong to the daemon, independently of phone delivery.
pub(crate) async fn run_alerts(
    state: Arc<RuntimeState>,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<()> {
    tokio::select! {
        _ = cancellation.cancelled() => return Ok(()),
        _ = mj_controller::upgrade::wait_for_startup_recovery() => {},
    }
    let mut revisions = state.revisions();
    let mut observer = NotificationReconciler::default();
    loop {
        revisions.borrow_and_update();
        let runtime = state.runtime_publication()?;
        let changed = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(()),
            result = observer.reconcile(runtime) => result?,
        };
        if let NotificationObservation::Observed(changed) = changed {
            if changed {
                state.publish_revision();
            }
            wake();
        }
        if !wait_for_notification_change(changed, &mut revisions, &cancellation).await? {
            return Ok(());
        }
    }
}

pub(crate) async fn run(state: Arc<RuntimeState>) -> Result<()> {
    // Let the startup owner settle before the first map can consume cutover.
    mj_controller::upgrade::wait_for_startup_recovery().await;
    let vapid = Arc::new(blocking(Vapid::load).await?);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut revisions = state.revisions();
    let mut sends = tokio::task::JoinSet::new();
    blocking(db::push_recover).await?;
    loop {
        let runtime = state.runtime_publication()?;
        let (due, next) = blocking(move || db::push_ready(runtime)).await?;
        for delivery in due {
            let client = client.clone();
            let vapid = vapid.clone();
            sends.spawn(async move { deliver(&client, &vapid, delivery).await });
        }
        // Serialize each device/tag so an older request cannot replace a newer
        // card. Other sessions and devices continue independently.
        let deadline = next.and_then(|at| {
            let delay = Duration::from_secs(
                at.saturating_sub(chrono::Utc::now().timestamp()).max(0) as u64,
            );
            // A service's Retry-After can exceed this clock's range.
            // In that case, the service waits for a state change.
            tokio::time::Instant::now().checked_add(delay)
        });
        tokio::select! {
            changed=revisions.changed()=>{changed.context("push runtime feed closed")?;},
            _=wake_signal().notified()=>{},
            completed=sends.join_next(),if !sends.is_empty()=>{let result=completed.context("push task disappeared")??;result?;},
            _=async {match deadline {Some(deadline)=>tokio::time::sleep_until(deadline).await,None=>std::future::pending().await}}=>{},
        }
    }
}
async fn deliver(
    client: &reqwest::Client,
    vapid: &Vapid,
    delivery: db::PushDelivery,
) -> Result<()> {
    let endpoint = delivery.subscription.endpoint.clone();
    let identity = delivery.identity.clone();
    let body = encrypt(&delivery.subscription, delivery.payload.as_bytes())?;
    // At-least-once: a crash after remote acceptance but before this receipt can
    // repeat a send. Stable session tags replace the card; they cannot prevent
    // a second alert after dismissal. Never discard unknown outcomes.
    let response = client
        .post(&endpoint)
        .header("Authorization", vapid.authorization(&endpoint)?)
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        // RFC 8030 requires TTL. Retain notifications for one day when a phone is offline.
        .header("TTL", "86400")
        .body(body)
        .send()
        .await;
    let mut service_status = None;
    let mut retry_after = None;
    let error;
    match response {
        Ok(response) => {
            let status = response.status();
            service_status = Some(status.as_u16());
            let accepted = status.is_success();
            error = (!accepted).then(|| format!("push service returned {status}"));
            if status.as_u16() == 429 || status.is_server_error() {
                retry_after = response
                    .headers()
                    .get("Retry-After")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|value| {
                        value
                            .parse::<i64>()
                            .ok()
                            .map(|seconds| {
                                chrono::Utc::now()
                                    .timestamp()
                                    .saturating_add(seconds.max(0))
                            })
                            .or_else(|| {
                                chrono::DateTime::parse_from_rfc2822(value)
                                    .ok()
                                    .map(|date| date.timestamp())
                            })
                    });
            } else if !accepted {
                // Permanent rejection: keep the error visible until re-enrollment.
                retry_after = Some(i64::MAX);
            }
        }
        Err(failure) => {
            // Subscription URLs are capabilities: retain diagnostics without them.
            error = Some(format!(
                "push transport failed; outcome unknown: {}",
                failure.without_url()
            ));
        }
    }
    if let Some(error) = &error {
        tracing::warn!(%error,"Web Push delivery deferred");
    }
    // Exponential backoff has no guessed retry cap. The persisted due time
    // prevents hot retry loops when the service omits Retry-After.
    let seconds = 1_i64.checked_shl(delivery.failures).unwrap_or(i64::MAX);
    let at = retry_after.unwrap_or_else(|| chrono::Utc::now().timestamp().saturating_add(seconds));
    blocking(move || {
        db::push_result(
            delivery.subscription,
            delivery.enrollment,
            identity,
            service_status,
            at,
            error,
        )
    })
    .await
}

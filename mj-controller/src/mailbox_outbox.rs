//! Single daemon owner for durable mailbox delivery to relay workers.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use mj_core::relay::RelayCommand;
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::daemon::RuntimeState;
use crate::session_manager::SessionManagerControl;

const TRUSTED_PARENT_MESSAGE_PROTOCOL_ERROR: &str = "This queued parent message reached an older mj worker that cannot receive structured mailbox events; it was not delivered.";
const PENDING_BATCH: usize = 256;
const SESSION_CONCURRENCY: usize = 8;
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(15);
const WORKER_SWEEP: Duration = Duration::from_secs(5);

fn outbox_notify() -> &'static Notify {
    static NOTIFY: OnceLock<Notify> = OnceLock::new();
    NOTIFY.get_or_init(Notify::new)
}

/// Wake the delivery owner after a producer commits new outbox rows.
pub(crate) fn notify_mailbox_outbox_changed() {
    outbox_notify().notify_one();
}

/// Retry durable events when they are enqueued and periodically after workers
/// attach. This service holds no upgrade admission: every accepted command has
/// a stable id and stays pending until the worker acknowledges it.
pub(crate) async fn run(
    state: std::sync::Arc<RuntimeState>,
    sessions: SessionManagerControl,
    stop: CancellationToken,
) -> Result<()> {
    let mut sweep = tokio::time::interval(WORKER_SWEEP);
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return Ok(()),
            _ = outbox_notify().notified() => {},
            _ = sweep.tick() => {},
        }
        if stop.is_cancelled() {
            return Ok(());
        }
        if let Err(error) = process_pending(state.clone(), sessions.clone(), stop.clone()).await {
            tracing::warn!(error = %format!("{error:#}"), "mailbox outbox sweep failed; pending rows remain durable");
        }
    }
}

async fn process_pending(
    state: std::sync::Arc<RuntimeState>,
    sessions: SessionManagerControl,
    stop: CancellationToken,
) -> Result<()> {
    let pruned_stopped_children =
        tokio::task::spawn_blocking(crate::database::prune_pending_messages_for_stopped_children)
            .await
            .context("stopped child message cleanup task failed")??;
    if pruned_stopped_children > 0 {
        tracing::info!(
            count = pruned_stopped_children,
            "removed pending mailbox messages for stopped children"
        );
    }
    if !state
        .controller_projection()
        .config
        .agent_mailboxes_enabled()
    {
        return Ok(());
    }
    let pruned =
        tokio::task::spawn_blocking(crate::database::prune_mailbox_events_for_missing_sessions)
            .await
            .context("mailbox orphan cleanup task failed")??;
    if pruned > 0 {
        tracing::info!(
            count = pruned,
            "removed mailbox events for destroyed sessions"
        );
    }
    let entries =
        tokio::task::spawn_blocking(|| crate::database::pending_mailbox_events(PENDING_BATCH))
            .await
            .context("mailbox outbox read task failed")??;
    let mut by_session = BTreeMap::new();
    for entry in entries {
        by_session
            .entry(entry.target_session_id.clone())
            .or_insert_with(Vec::new)
            .push(entry);
    }

    let mut work = by_session.into_iter();
    let mut active = JoinSet::new();
    loop {
        while active.len() < SESSION_CONCURRENCY {
            let Some((session_id, events)) = work.next() else {
                break;
            };
            let state = state.clone();
            let sessions = sessions.clone();
            let stop = stop.clone();
            active.spawn(async move {
                deliver_session_events(state, sessions, stop, session_id, events).await
            });
        }
        if active.is_empty() {
            break;
        }
        tokio::select! {
            _ = stop.cancelled() => {
                active.abort_all();
                while let Some(result) = active.join_next().await {
                    if let Err(error) = result && !error.is_cancelled() {
                        tracing::warn!(%error, "mailbox delivery task failed during shutdown");
                    }
                }
                return Ok(());
            }
            result = active.join_next() => {
                match result.context("mailbox delivery task disappeared")? {
                    Ok(()) => {}
                    Err(error) => tracing::warn!(%error, "mailbox delivery task panicked"),
                }
            }
        }
    }
    Ok(())
}

pub(crate) async fn deliver_session_events(
    state: std::sync::Arc<dyn crate::server_runtime::api::ExportRuntime>,
    sessions: SessionManagerControl,
    stop: CancellationToken,
    session_id: String,
    events: Vec<crate::database::MailboxOutboxEntry>,
) {
    for entry in events {
        if stop.is_cancelled() {
            return;
        }
        let event = match serde_json::from_str::<mj_core::mailbox::MailboxEvent>(&entry.event_json)
        {
            Ok(event) => event,
            Err(error) => {
                tracing::error!(%session_id, event_key = %entry.event_key, %error, "mailbox outbox row contains an invalid event");
                continue;
            }
        };
        let command_id = mailbox_command_id(&entry.event_key);
        match deliver_mailbox_event(state.clone(), &sessions, &session_id, event, entry.unpark)
            .await
        {
            Ok(MailboxEventDelivery::Accepted(ordinal)) => {
                let key = entry.event_key.clone();
                let mark_id = command_id.clone();
                match tokio::task::spawn_blocking(move || {
                    crate::database::mark_mailbox_event_accepted(&key, &mark_id, ordinal)
                })
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::warn!(%session_id, event_key = %entry.event_key, %error, "mailbox command was accepted but its outbox acknowledgement was not recorded; it will retry with the same command id")
                    }
                    Err(error) => {
                        tracing::warn!(%session_id, event_key = %entry.event_key, %error, "mailbox acknowledgement task failed; it will retry with the same command id")
                    }
                }
            }
            Ok(MailboxEventDelivery::Pending) => return,
            Ok(MailboxEventDelivery::Failed(reason)) => {
                let key = entry.event_key.clone();
                let persisted_reason = reason.clone();
                match tokio::task::spawn_blocking(move || {
                    crate::database::mark_mailbox_event_failed(&key, &persisted_reason)
                })
                .await
                {
                    Ok(Ok(())) => tracing::warn!(
                        %session_id,
                        event_key = %entry.event_key,
                        %reason,
                        "mailbox event cannot be delivered to this worker and was marked failed"
                    ),
                    Ok(Err(error)) => {
                        tracing::warn!(%session_id, event_key = %entry.event_key, %error, "failed to persist mailbox delivery failure; event remains pending");
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(%session_id, event_key = %entry.event_key, %error, "mailbox failure persistence task failed; event remains pending");
                        return;
                    }
                }
            }
            Err(error) => {
                tracing::debug!(%session_id, event_key = %entry.event_key, %command_id, %error, "mailbox delivery is pending and will retry with the same command id");
                return;
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MailboxEventDelivery {
    Pending,
    Accepted(u64),
    Failed(String),
}

/// Read the worker protocol only from the protocol version published in the
/// session manager's current view. Request admission and outbox delivery use
/// this same fact.
pub(crate) fn published_worker_relay_protocol(
    view: &mj_client::session::ManagedSessionView,
) -> Option<u32> {
    view.snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.operational.relay_protocol_version)
}

pub(crate) fn trusted_parent_message_protocol_error(protocol: Option<u32>) -> Option<&'static str> {
    protocol
        .filter(|version| *version < mj_core::relay::RELAY_STRUCTURED_MAILBOX_PROTOCOL)
        .map(|_| TRUSTED_PARENT_MESSAGE_PROTOCOL_ERROR)
}

/// Submit one durable event through the existing session actor. Callers may
/// opt into unpark for deliberate parent-to-child messages; GitHub and the
/// public events API always pass `false`.
pub(crate) async fn deliver_mailbox_event(
    state: std::sync::Arc<dyn crate::server_runtime::api::ExportRuntime>,
    sessions: &SessionManagerControl,
    session_id: &str,
    event: mj_core::mailbox::MailboxEvent,
    unpark_parked: bool,
) -> Result<MailboxEventDelivery> {
    let Some(record) = state.session_record(session_id) else {
        return Ok(MailboxEventDelivery::Pending);
    };
    if record.state == mj_core::state::SessionState::Parked {
        if !unpark_parked {
            return Ok(MailboxEventDelivery::Pending);
        }
        std::sync::Arc::clone(&state)
            .unpark_subagent(session_id.to_owned())
            .await?;
    }
    let Some(record) = state.session_record(session_id) else {
        return Ok(MailboxEventDelivery::Pending);
    };
    if !record.state.has_live_worker() {
        return Ok(MailboxEventDelivery::Pending);
    }
    let Some(handle) = sessions.find_session(session_id.to_owned()).await? else {
        return Ok(MailboxEventDelivery::Pending);
    };
    let view = handle.view();
    let protocol = published_worker_relay_protocol(&view);
    if matches!(
        &event.body,
        mj_core::mailbox::MailboxEventBody::ParentMessage { .. }
    ) && let Some(reason) = trusted_parent_message_protocol_error(protocol)
    {
        return Ok(MailboxEventDelivery::Failed(reason.to_owned()));
    }
    if !view.connected
        || protocol.is_none_or(|version| version < mj_core::relay::RELAY_LEGACY_MAILBOX_PROTOCOL)
    {
        return Ok(MailboxEventDelivery::Pending);
    }
    let command_id = mailbox_command_id(&event.key);
    match tokio::time::timeout(
        DELIVERY_TIMEOUT,
        handle
            .client()
            .submit(command_id, RelayCommand::DeliverMailboxEvent { event }),
    )
    .await
    {
        Ok(result) => result.map(MailboxEventDelivery::Accepted),
        Err(_) => anyhow::bail!("mailbox command acknowledgement timed out"),
    }
}

pub(crate) fn mailbox_command_id(event_key: &str) -> String {
    let digest = Sha256::digest(event_key.as_bytes());
    let mut command_id = String::with_capacity("mailbox-".len() + digest.len() * 2);
    command_id.push_str("mailbox-");
    for byte in digest {
        use std::fmt::Write as _;
        write!(command_id, "{byte:02x}").expect("writing to String cannot fail");
    }
    command_id
}

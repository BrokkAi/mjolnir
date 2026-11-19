//! Durable Web Push reservations and receipts, owned by the dispatch service.
use super::*;
use crate::web_push::{Notice, PushEntry, Subscription, card_entries};
use mj_client::runtime_feed::RuntimeProjection;

pub(crate) fn push_subscribe(subscription: Subscription) -> Result<()> {
    submit_database_write("subscribe Web Push device", move |db| {
        push_subscribe_with(db, subscription)
    })
}
fn push_subscribe_with(db: &Connection, subscription: Subscription) -> Result<()> {
    let tx = db.unchecked_transaction()?;
    push_subscribe_in(&tx, subscription)?;
    tx.commit()?;
    Ok(())
}
fn push_subscribe_in(db: &Connection, subscription: Subscription) -> Result<()> {
    let encoded = serde_json::to_string(&subscription)?;
    let previous: Option<(String, String)> = db
        .query_row(
            "SELECT subscription,enrollment FROM push_devices WHERE endpoint=?1",
            [&subscription.endpoint],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let previous = previous
        .map(|(old, enrollment)| {
            Ok::<_, anyhow::Error>((serde_json::from_str::<Subscription>(&old)?, enrollment))
        })
        .transpose()?;
    let enrollment = match previous {
        // Expiry metadata does not change the encryption capability: keep its
        // owner so an already admitted response can still complete normally.
        Some((old, enrollment)) if old.keys == subscription.keys => enrollment,
        _ => {
            // Replacement releases the old owner's reservations in the same
            // transaction. An opaque ID also prevents ABA after delete/recreate.
            db.execute(
                "UPDATE push_entry_deliveries SET accepted=0,flight_session=NULL,retry_at=0,failures=0,error=NULL WHERE endpoint=?1 AND accepted IN (0,3)",
                [&subscription.endpoint],
            )?;
            mj_core::state::new_session_id().context("generate Web Push enrollment identifier")?
        }
    };
    db.execute("INSERT INTO push_devices(endpoint,subscription,enrollment) VALUES(?1,?2,?3) ON CONFLICT(endpoint) DO UPDATE SET subscription=excluded.subscription,enrollment=excluded.enrollment",params![subscription.endpoint,encoded,enrollment])?;
    // Routine device reconciliation must preserve retry deadlines and errors.
    Ok(())
}
pub(crate) fn push_renew(old_endpoint: String, subscription: Subscription) -> Result<()> {
    submit_database_write("renew Web Push subscription", move |db| {
        push_renew_with(db, &old_endpoint, subscription)
    })
}
fn push_renew_with(db: &Connection, old_endpoint: &str, subscription: Subscription) -> Result<()> {
    if old_endpoint == subscription.endpoint {
        return push_subscribe_with(db, subscription);
    }
    let tx = db.unchecked_transaction()?;
    push_subscribe_in(&tx, subscription.clone())?;
    // Preserve known receipts across native endpoint rotation. Old in-flight
    // attempts become uncertain queued work under the new encryption owner.
    tx.execute("UPDATE push_devices SET retry_at=max(retry_at,coalesce((SELECT retry_at FROM push_devices WHERE endpoint=?1),0)) WHERE endpoint=?2",params![old_endpoint,subscription.endpoint])?;
    tx.execute("INSERT INTO push_entry_deliveries(endpoint,identity,accepted,retry_at,failures,error)
        SELECT ?2,q.identity,CASE WHEN q.accepted=3 THEN 0 ELSE q.accepted END,q.retry_at,q.failures,q.error
        FROM push_entry_deliveries q WHERE q.endpoint=?1
        ON CONFLICT(endpoint,identity) DO UPDATE SET accepted=CASE WHEN push_entry_deliveries.accepted=3 THEN 3 WHEN push_entry_deliveries.accepted=1 OR excluded.accepted=1 THEN 1 ELSE push_entry_deliveries.accepted END",params![old_endpoint,subscription.endpoint])?;
    tx.execute("DELETE FROM push_devices WHERE endpoint=?1", [old_endpoint])?;
    tx.commit()?;
    Ok(())
}
pub(crate) fn push_unsubscribe(endpoint: &str) -> Result<()> {
    let endpoint = endpoint.to_owned();
    submit_database_write("unsubscribe Web Push device", move |db| {
        push_unsubscribe_with(db, &endpoint)
    })
}
fn push_unsubscribe_with(db: &Connection, endpoint: &str) -> Result<()> {
    db.execute("DELETE FROM push_devices WHERE endpoint=?1", [endpoint])?;
    Ok(())
}

/// Read a consistent durable frontier and only the events this client needs.
/// No retention cap: a cursor may resume after any length of disconnection.
pub(crate) fn alert_events_since(
    after: Option<u64>,
) -> Result<(u64, Vec<mj_client::runtime_feed::AlertEvent>)> {
    let db = open_reader(&database_path())?;
    alert_events_since_with(&db, after)
}
fn alert_events_since_with(
    db: &Connection,
    after: Option<u64>,
) -> Result<(u64, Vec<mj_client::runtime_feed::AlertEvent>)> {
    let tx = db.unchecked_transaction()?;
    let high = tx.query_row(
        "SELECT coalesce((SELECT seq FROM sqlite_sequence WHERE name='alert_events'),0)",
        [],
        |r| r.get::<_, u64>(0),
    )?;
    let events = tx.prepare("SELECT sequence,destination,session_title,text,kind,identity FROM alert_events WHERE sequence>?1 AND sequence<=?2 ORDER BY sequence")?
        .query_map(params![after.unwrap_or(high), high], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get::<_,String>(4)?,r.get(5)?)))?
        .map(|row| { let (sequence,destination,session_title,text,kind,identity) = row?; Ok(mj_client::runtime_feed::AlertEvent { sequence,destination,session_title,text,identity,kind: serde_json::from_str(&kind)? }) })
        .collect::<Result<Vec<_>>>()?;
    tx.commit()?;
    Ok((high, events))
}

/// The daemon notification service is the sole observer of published items.
pub(crate) struct NotificationReconcile {
    pub alerts_changed: bool,
    pub complete: bool,
}

pub(crate) fn notification_reconcile(
    runtime: RuntimeProjection,
) -> Result<Option<NotificationReconcile>> {
    // The blocking owner holds admission from queueing through commit. A drain
    // refuses this disposable observation; the next daemon rebuilds it.
    let Ok(_work) = crate::upgrade::activity_unless_draining("notification reconciliation") else {
        return Ok(None);
    };
    submit_database_write("observe notification entries", move |db| {
        let before = alert_events_since_with(db, None)?.0;
        let tx = db.transaction()?;
        let complete = if let Some(entries) = card_entries(&runtime) {
            push_observe_in(&tx, &runtime, &entries, Utc::now().timestamp())?
        } else {
            true
        };
        tx.commit()?;
        Ok(Some(NotificationReconcile {
            alerts_changed: alert_events_since_with(db, None)?.0 != before,
            complete,
        }))
    })
}

pub(crate) fn push_ready(runtime: RuntimeProjection) -> Result<(Vec<PushDelivery>, Option<i64>)> {
    submit_database_write("claim Web Push deliveries", move |db| {
        push_reconcile_with(db, &runtime, Utc::now().timestamp(), false)
    })
}

/// Reconcile the authoritative card snapshot and admit its sends in one
/// transaction. Presence controls delivery and never changes entry identity.
#[cfg(test)]
pub(crate) fn push_reconcile(
    runtime: RuntimeProjection,
) -> Result<(Vec<PushDelivery>, Option<i64>, bool)> {
    submit_database_write("reconcile Web Push entries", move |db| {
        let before = alert_events_since_with(db, None)?.0;
        let (due, next) = push_reconcile_with(db, &runtime, Utc::now().timestamp(), true)?;
        let changed = alert_events_since_with(db, None)?.0 != before;
        Ok((due, next, changed))
    })
}

fn push_reconcile_with(
    db: &mut Connection,
    runtime: &RuntimeProjection,
    now: i64,
    observe: bool,
) -> Result<(Vec<PushDelivery>, Option<i64>)> {
    let entries = card_entries(runtime);
    let automatic = runtime.metadata.operator_presence.away && entries.is_some();
    let tx = db.transaction()?;
    if let Some(entries) = &entries
        && observe
    {
        push_observe_in(&tx, runtime, entries, now)?;
    }
    let due = push_claim_in(&tx, runtime, entries.as_ref(), automatic, now)?;
    let next = tx.query_row(
        // Ready entries absent from this snapshot await the observer's wake,
        // rather than spinning on an already-expired delivery deadline.
        &format!("{READY_PUSH} SELECT min(due) FROM ranked WHERE rank=1 AND due>?2"),
        params![automatic, now],
        |r| r.get(0),
    )?;
    tx.commit()?;
    Ok((due, next))
}

fn push_observe_in(
    tx: &Transaction<'_>,
    runtime: &RuntimeProjection,
    entries: &BTreeMap<String, PushEntry<&str>>,
    now: i64,
) -> Result<bool> {
    let mut complete = true;
    // The cutover baseline applies even to devices enrolled later. An absent
    // card map cannot consume it, and a locator still in flight has no identity.
    let baseline = tx.execute("INSERT OR IGNORE INTO push_entry_cutover VALUES(1)", [])? == 1;
    let visible = tx
        .prepare("SELECT identity FROM push_entries WHERE visible=1 AND session IS NOT NULL")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut hide = tx.prepare("UPDATE push_entries SET visible=0 WHERE identity=?1")?;
    for id in visible {
        if !entries.contains_key(&id) {
            hide.execute([id])?;
        }
    }
    let mut upsert = tx.prepare(
        "INSERT INTO push_entries(identity,session,completed_ordinal,visible,baseline,first_seen_at_ms)
         VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(identity) DO UPDATE SET session=excluded.session,
             completed_ordinal=excluded.completed_ordinal,visible=excluded.visible
         WHERE session IS NOT excluded.session OR completed_ordinal IS NOT excluded.completed_ordinal
             OR visible IS NOT excluded.visible",
    )?;
    for (identity, entry) in entries {
        upsert.execute(params![
            identity,
            entry.session,
            entry.completed_ordinal,
            !entry.paused,
            baseline,
            now.saturating_mul(1_000),
        ])?;
    }
    // The same identity reservation owns both terminal alerts and push delivery.
    // Presence and device enrollment do not participate in this decision.
    for (identity, entry) in entries {
        if entry.paused {
            continue;
        }
        let eligible: bool = tx.query_row(
            "SELECT baseline=0 AND NOT EXISTS(SELECT 1 FROM alert_baseline WHERE identity=?1) AND NOT EXISTS(SELECT 1 FROM alert_events WHERE identity=?1) FROM push_entries WHERE identity=?1",
            [identity],
            |r| r.get(0),
        )?;
        if !eligible {
            continue;
        }
        let message = match push_entry_text(tx, runtime, entry) {
            Ok(message) => message,
            Err(error) => {
                tracing::warn!(session = entry.session, %identity, error = format!("{error:#}"), "could not read notification text; leaving item pending");
                complete = false;
                continue;
            }
        };
        let notice = entry.notice(runtime, identity.clone(), message.as_deref());
        tx.execute(
            "INSERT INTO alert_events(identity,destination,session_title,text,kind) VALUES(?1,?2,?3,?4,?5)",
            params![identity, entry.holder, notice.title, notice.body, serde_json::to_string(&entry.kind)?],
        )?;
    }
    Ok(complete)
}

pub(crate) fn push_test(endpoint: String) -> Result<()> {
    submit_database_write("reserve Web Push test", move |db| {
        push_test_with(db, endpoint)
    })
}
fn push_test_with(db: &mut Connection, endpoint: String) -> Result<()> {
    let tx = db.transaction()?;
    ensure!(
        tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM push_devices WHERE endpoint=?1)",
            [&endpoint],
            |r| r.get::<_, bool>(0)
        )?,
        "this device is not subscribed"
    );
    let now = Utc::now();
    let identity = format!("test:{}", now.timestamp_nanos_opt().unwrap_or_default());
    tx.execute(
        "INSERT OR IGNORE INTO push_entries(identity,visible,baseline,first_seen_at_ms) VALUES(?1,1,0,?2)",
        params![identity, now.timestamp_millis()],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO push_entry_deliveries(endpoint,identity) VALUES(?1,?2)",
        params![endpoint, identity],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) struct PushDelivery {
    pub subscription: Subscription,
    pub enrollment: String,
    pub identity: String,
    pub payload: String,
    pub failures: u32,
}

// Rank before checking deadlines so an older retry holds its device/tag while
// other tags can proceed. A permanent rejection is retired until re-enrollment.
// The flight keeps the tag it sent even if a live form moves to another card.
const READY_PUSH: &str = "WITH ranked AS (
 SELECT d.subscription,d.enrollment,q.identity,q.failures,e.session,
        max(q.retry_at,d.retry_at) AS due,
        row_number() OVER (
            PARTITION BY q.endpoint,e.session
            ORDER BY e.completed_ordinal IS NULL,e.completed_ordinal,e.identity
        ) AS rank
 FROM push_entry_deliveries q JOIN push_devices d USING(endpoint) JOIN push_entries e USING(identity)
 WHERE q.accepted=0 AND q.retry_at<9223372036854775807
   AND e.visible=1 AND (e.session IS NULL OR ?1)
   AND NOT EXISTS(
       SELECT 1 FROM push_entry_deliveries flight
       WHERE flight.endpoint=q.endpoint AND flight.accepted=3
         AND flight.flight_session IS e.session
   )
)";

fn push_claim_in(
    tx: &Transaction<'_>,
    runtime: &RuntimeProjection,
    entries: Option<&BTreeMap<String, PushEntry<&str>>>,
    automatic: bool,
    now: i64,
) -> Result<Vec<PushDelivery>> {
    if automatic {
        tx.execute(
            "INSERT OR IGNORE INTO push_entry_deliveries(endpoint,identity)
             SELECT d.endpoint,e.identity FROM push_entries e CROSS JOIN push_devices d
             WHERE e.visible=1 AND e.session IS NOT NULL AND e.baseline=0",
            [],
        )?;
    }
    let due = tx
        .prepare(&format!(
            "{READY_PUSH} SELECT subscription,enrollment,identity,failures,session
         FROM ranked WHERE rank=1 AND due<=?2"
        ))?
        .query_map(params![automatic, now], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, u32>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut admitted = Vec::new();
    let mut notices = BTreeMap::new();
    for (subscription, enrollment, identity, failures, session) in due {
        let subscription: Subscription = serde_json::from_str(&subscription)?;
        let payload = if session.is_some() {
            let Some(entry) = entries.and_then(|entries| entries.get(&identity)) else {
                // The delivery snapshot can advance before the notification
                // owner hides its ledger row. Its reconciliation wakes us.
                continue;
            };
            if entry.paused {
                // Availability can change before the observer hides the ledger
                // row. The consumer must not admit that stale decision.
                continue;
            }
            if let Some(payload) = notices.get(&identity) {
                String::clone(payload)
            } else {
                let message = push_entry_text(tx, runtime, entry)?;
                let payload = serde_json::to_string(&entry.notice(
                    runtime,
                    identity.clone(),
                    message.as_deref(),
                ))?;
                notices.insert(identity.clone(), payload.clone());
                payload
            }
        } else {
            serde_json::to_string(&Notice {
                title: "Mjolnir".into(),
                body: "Notifications are enabled on this device".into(),
                session_id: None,
                url: "/".into(),
                tag: "test".into(),
                episode: None,
                observed_revision: 0,
            })?
        };
        tx.execute(
            "UPDATE push_entry_deliveries SET accepted=3,flight_session=?3 WHERE endpoint=?1 AND identity=?2",
            params![subscription.endpoint, identity, session],
        )?;
        admitted.push(PushDelivery {
            subscription,
            enrollment,
            identity,
            payload,
            failures,
        });
    }
    Ok(admitted)
}

fn push_entry_text(
    db: &Connection,
    runtime: &RuntimeProjection,
    entry: &PushEntry<&str>,
) -> Result<Option<String>> {
    use mj_core::transcript::materialized_agent_message_text;
    if let Some(text) = entry.text {
        return Ok(Some(text.to_owned()));
    }
    if let Some(id) = entry.message_id {
        let tail = runtime.transcripts.get(entry.session);
        if let Some(text) = materialized_agent_message_text(
            tail.into_iter()
                .flat_map(|tail| tail.items.values())
                .map(AsRef::as_ref),
            id,
        ) {
            return Ok(Some(text));
        }
        let stored = load_materialized_transcript_item(entry.session, id)?;
        return Ok(materialized_agent_message_text(stored.iter(), id));
    }
    // An entry without a message id reads its own turn only, as the cards do.
    let Some(through) = entry.completed_ordinal else {
        return Ok(None);
    };
    let item = materialized::materialized_agent_item_through(db, entry.session, through)?;
    Ok(item
        .and_then(|item| materialized_agent_message_text(std::iter::once(&item), &item.stable_id)))
}

pub(crate) fn push_recover() -> Result<()> {
    submit_database_write("recover uncertain Web Push sends", |db| {
        push_recover_with(db)
    })
}
fn push_recover_with(db: &Connection) -> Result<()> {
    // Only service startup, under the daemon's store ownership, recovers flight
    // reservations. Remote acceptance may be unknown; receipts never reset.
    db.execute(
        "UPDATE push_entry_deliveries SET accepted=0,flight_session=NULL WHERE accepted=3",
        [],
    )?;
    Ok(())
}
pub(crate) fn push_result(
    subscription: Subscription,
    enrollment: String,
    identity: String,
    status: Option<u16>,
    retry_at: i64,
    error: Option<String>,
) -> Result<()> {
    submit_database_write("record Web Push receipt", move |db| {
        apply_push_result_with(
            db,
            subscription,
            &enrollment,
            identity,
            status,
            retry_at,
            error,
        )
    })
}
fn apply_push_result_with(
    db: &Connection,
    subscription: Subscription,
    enrollment: &str,
    identity: String,
    status: Option<u16>,
    retry_at: i64,
    error: Option<String>,
) -> Result<()> {
    let tx = db.unchecked_transaction()?;
    let current: Option<String> = tx
        .query_row(
            "SELECT enrollment FROM push_devices WHERE endpoint=?1",
            [&subscription.endpoint],
            |r| r.get(0),
        )
        .optional()?;
    if current.as_deref() != Some(enrollment) {
        return Ok(());
    }
    if matches!(status, Some(404 | 410)) {
        // A response from an older enrollment cannot delete its replacement.
        tx.execute(
            "DELETE FROM push_devices WHERE endpoint=?1 AND enrollment=?2",
            params![subscription.endpoint, enrollment],
        )?;
        tx.commit()?;
        return Ok(());
    }
    let owns_flight:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM push_entry_deliveries WHERE endpoint=?1 AND identity=?2 AND accepted=3)",params![subscription.endpoint,identity],|r|r.get(0))?;
    if !owns_flight {
        return Ok(());
    }
    // A permanent payload rejection retires only that intent. Endpoint-wide
    // backpressure is reserved for transient service/transport failures.
    if status.is_none() || status.is_some_and(|status| status == 429 || status >= 500) {
        // Retry-After constrains follow-up requests to this endpoint, including
        // newly queued entries, not just the rejected intent.
        tx.execute(
            "UPDATE push_devices SET retry_at=max(retry_at,?2) WHERE endpoint=?1",
            params![subscription.endpoint, retry_at],
        )?;
    }
    push_result_with(
        &tx,
        subscription.endpoint,
        identity,
        status.is_some_and(|status| (200..300).contains(&status)),
        retry_at,
        error,
    )?;
    tx.commit()?;
    Ok(())
}
fn push_result_with(
    db: &Connection,
    endpoint: String,
    identity: String,
    accepted: bool,
    retry_at: i64,
    error: Option<String>,
) -> Result<()> {
    db.execute("UPDATE push_entry_deliveries SET accepted=?3,flight_session=NULL,retry_at=?4,failures=failures+1,error=?5 WHERE endpoint=?1 AND identity=?2 AND accepted IN (0,3)",params![endpoint,identity,accepted,retry_at,error])?;
    Ok(())
}
pub(crate) fn push_device_status(endpoint: String) -> Result<serde_json::Value> {
    let db = open_reader(&database_path())?;
    let subscribed = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM push_devices WHERE endpoint=?1)",
        [&endpoint],
        |r| r.get::<_, bool>(0),
    )?;
    let error:Option<String>=db.query_row("SELECT error FROM push_entry_deliveries WHERE endpoint=?1 AND accepted IN (0,3) AND error IS NOT NULL ORDER BY retry_at DESC LIMIT 1",[endpoint],|r|r.get(0)).optional()?.flatten();
    Ok(serde_json::json!({"subscribed":subscribed,"error":error}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_client::runtime_feed::{CardForm, NotificationCard, SessionTail};
    use mj_core::attention::{
        EntryMessage, NotificationKind, NotificationStackEntry, SessionAttention,
    };

    fn subscription() -> Subscription {
        Subscription {
            endpoint: "https://push.example.com/device".into(),
            keys: crate::web_push::SubscriptionKeys {
                p256dh: "key".into(),
                auth: "auth".into(),
            },
            expiration_time: None,
        }
    }

    fn runtime(away: bool) -> RuntimeProjection {
        let mut runtime = RuntimeProjection::default();
        runtime.metadata.operator_presence.away = away;
        runtime.metadata.notification_cards = Some(Default::default());
        runtime.revision = 40;
        runtime
    }

    fn show(runtime: &mut RuntimeProjection, session: &str, entries: Vec<NotificationStackEntry>) {
        runtime
            .metadata
            .notification_cards
            .as_mut()
            .unwrap()
            .insert(
                session.into(),
                NotificationCard {
                    since_ms: 0,
                    badge: NotificationKind::NeedsInput,
                    entries,
                    dismissal: None,
                    holder: None,
                    form: None,
                },
            );
    }

    fn completion(ordinal: u64, message: EntryMessage) -> NotificationStackEntry {
        NotificationStackEntry {
            completed_at_ms: 0,
            completed_ordinal: ordinal,
            kind: NotificationKind::NeedsInput,
            message,
        }
    }

    fn turn_only(runtime: &mut RuntimeProjection, ids: &[&str]) {
        runtime
            .metadata
            .notification_cards
            .as_mut()
            .unwrap()
            .clear();
        for id in ids {
            show(runtime, id, vec![completion(1, EntryMessage::None)]);
        }
    }

    fn result(db: &Connection, delivery: &PushDelivery, status: Option<u16>, retry_at: i64) {
        apply_push_result_with(
            db,
            delivery.subscription.clone(),
            &delivery.enrollment,
            delivery.identity.clone(),
            status,
            retry_at,
            None,
        )
        .unwrap();
    }

    fn claim(db: &mut Connection, runtime: &RuntimeProjection, now: i64) -> Vec<PushDelivery> {
        push_reconcile_with(db, runtime, now, true).unwrap().0
    }

    fn add_message(transcript: &mut MaterializedSession, id: &str, ordinal: u64, text: &str) {
        transcript.transcript.push(Arc::new(TranscriptItem {
            stable_id: id.into(),
            position: ordinal,
            latest_content_event_ordinal: Some(ordinal),
            created_at_ms: ordinal as i64,
            last_changed_at_ms: ordinal as i64,
            body: TranscriptBody::Agent {
                chunks: vec![serde_json::json!({"content":{"type":"text","text":text}})],
                streaming: false,
            },
        }));
    }

    fn finished_turn(start: u64, completed: u64) -> MaterializedTurnOutcome {
        MaterializedTurnOutcome {
            command_id: format!("turn-{completed}"),
            accepted_ordinal: Some(start),
            turn_start_position: Some(start),
            completed_ordinal: completed,
            completed_at_ms: completed as i64,
            outcome: mj_core::state::TurnOutcomeKind::Completed {
                stop_reason: "EndTurn".into(),
            },
            answered_through: None,
            diagnostic: None,
            usage: None,
            work_facts: None,
        }
    }

    fn save_turn_history(db: &Connection, session: &str, turn: &MaterializedTurnOutcome) {
        db.execute(
            "INSERT INTO session_turn_usage(session_id,command_id,completed_ordinal,turn_start_position,body)
             VALUES(?1,?2,?3,?4,?5)",
            params![session, turn.command_id, turn.completed_ordinal, turn.turn_start_position,
                serialize_turn_outcome(turn).unwrap()],
        ).unwrap();
    }

    // Hard-won: integrated review of a1f2e5f67: push read an id-less entry's text from an earlier turn.
    #[test]
    fn push_id_less_entry_reads_only_its_own_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("push-test.sqlite3");
        let mut db = open(&path).unwrap();
        push_subscribe_with(&db, subscription()).unwrap();
        sessions::save_session_to(&path, &crate::database::tests::session("s", "project")).unwrap();

        let mut transcript = MaterializedSession::empty("s");
        transcript.applied_event_ordinal = 9;
        transcript.applied_event_digest = format!("{:064x}", 9);
        add_message(&mut transcript, "agent:previous", 4, "Earlier turn reply");
        transcript.last_turn_outcome = Some(finished_turn(8, 9));
        materialized::save_materialized_session_to(&path, &transcript).unwrap();
        save_turn_history(&db, "s", &finished_turn(3, 5));

        let mut runtime = runtime(true);
        assert!(claim(&mut db, &runtime, 0).is_empty()); // Cutover.
        show(
            &mut runtime,
            "s",
            vec![
                completion(5, EntryMessage::None),
                completion(9, EntryMessage::None),
            ],
        );
        let with_text = claim(&mut db, &runtime, 0);
        assert_eq!(with_text.len(), 1);
        let notice: Notice = serde_json::from_str(&with_text[0].payload).unwrap();
        assert_eq!(notice.body, "Earlier turn reply");
        result(&db, &with_text[0], Some(201), 0);

        let without_text = claim(&mut db, &runtime, 0);
        assert_eq!(without_text.len(), 1);
        let notice: Notice = serde_json::from_str(&without_text[0].payload).unwrap();
        assert_eq!(notice.body, "Needs your input");
    }

    #[test]
    fn push_locator_settles_once_across_working_restart_and_reclassification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("push-test.sqlite3");
        let mut db = open(&path).unwrap();
        push_subscribe_with(&db, subscription()).unwrap();
        let mut runtime = runtime(true);
        assert!(claim(&mut db, &runtime, 0).is_empty()); // Cutover.
        show(
            &mut runtime,
            "s",
            vec![completion(10, EntryMessage::Locating)],
        );
        runtime
            .metadata
            .session_attention
            .insert("child".into(), SessionAttention::NeedsInput);
        assert!(claim(&mut db, &runtime, 0).is_empty());

        let mut transcript = MaterializedSession::empty("s");
        add_message(&mut transcript, "agent:original", 9, "Keep the old format?");
        add_message(&mut transcript, "agent:latest", 20, "New unrelated reply");
        runtime.transcripts.insert(
            "s".into(),
            SessionTail::of(&transcript, &ProjectionWindow::of(&transcript), 10),
        );
        let mut entry = completion(10, EntryMessage::Found("agent:original".into()));
        show(&mut runtime, "s", vec![entry.clone()]);
        let due = claim(&mut db, &runtime, 0);
        assert_eq!(due.len(), 1);
        let notice: Notice = serde_json::from_str(&due[0].payload).unwrap();
        assert_eq!(notice.body, "Keep the old format?");
        let (high, events) = alert_events_since_with(&db, Some(0)).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].destination, "s");
        assert_eq!(events[0].text, notice.body);
        assert_eq!(events[0].sequence, high);
        assert_eq!(notice.episode.as_deref(), Some(due[0].identity.as_str()));
        assert_eq!(notice.observed_revision, 40);
        result(&db, &due[0], Some(201), 0);

        runtime
            .metadata
            .session_attention
            .insert("s".into(), SessionAttention::Working);
        turn_only(&mut runtime, &[]);
        assert!(claim(&mut db, &runtime, 0).is_empty());
        runtime
            .metadata
            .session_attention
            .insert("s".into(), SessionAttention::NeedsInput);
        show(&mut runtime, "s", vec![entry.clone()]);
        assert!(claim(&mut db, &runtime, 0).is_empty());

        drop(db);
        let mut db = open(&path).unwrap();
        push_recover_with(&db).unwrap();
        entry.kind = NotificationKind::Important;
        show(&mut runtime, "s", vec![entry]);
        assert!(claim(&mut db, &runtime, 0).is_empty());
        assert_eq!(alert_events_since_with(&db, Some(0)).unwrap().1.len(), 1);
        assert!(alert_events_since_with(&db, None).unwrap().1.is_empty());
        assert!(
            alert_events_since_with(&db, Some(high))
                .unwrap()
                .1
                .is_empty()
        );
    }

    #[test]
    fn alert_decision_is_independent_of_presence_devices_and_reused_form_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alerts.sqlite3");
        let mut db = open(&path).unwrap();
        let mut runtime = runtime(false);
        assert!(claim(&mut db, &runtime, 0).is_empty());
        let mut parent = crate::database::tests::session("parent", "project");
        parent.session_title_override = Some("Branch review".into());
        runtime.records.insert("parent".into(), parent);
        show(&mut runtime, "parent", vec![]);
        let card = runtime
            .metadata
            .notification_cards
            .as_mut()
            .unwrap()
            .get_mut("parent")
            .unwrap();
        card.holder = Some("child".into());
        card.form = Some(CardForm {
            raised_at_ms: 1,
            request_id: "elicitation-1".into(),
            message: "Which branch?".into(),
        });
        let mut holder = crate::database::tests::session("child", "project");
        holder.notifications_ignored = true;
        runtime.records.insert("child".into(), holder);
        assert!(claim(&mut db, &runtime, 1).is_empty());
        assert!(
            alert_events_since_with(&db, Some(0)).unwrap().1.is_empty(),
            "a muted holder cannot notify through its parent's card"
        );
        runtime
            .records
            .get_mut("child")
            .unwrap()
            .notifications_ignored = false;
        assert!(claim(&mut db, &runtime, 1).is_empty());
        let (high, events) = alert_events_since_with(&db, Some(0)).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].destination, "child");
        assert_eq!(events[0].text, "Which branch?");
        assert_eq!(events[0].session_title, "Branch review");
        let card = runtime
            .metadata
            .notification_cards
            .as_mut()
            .unwrap()
            .remove("parent")
            .unwrap();
        claim(&mut db, &runtime, 2);
        drop(db);
        let mut db = open(&path).unwrap();
        runtime
            .metadata
            .notification_cards
            .as_mut()
            .unwrap()
            .insert("parent".into(), card);
        claim(&mut db, &runtime, 3);
        assert_eq!(
            alert_events_since_with(&db, Some(0)).unwrap().0,
            high,
            "one live form remains one identity across a restart"
        );
        runtime
            .metadata
            .notification_cards
            .as_mut()
            .unwrap()
            .get_mut("parent")
            .unwrap()
            .form
            .as_mut()
            .unwrap()
            .raised_at_ms = 4;
        claim(&mut db, &runtime, 4);
        claim(&mut db, &runtime, 5);
        let (new_high, events) = alert_events_since_with(&db, Some(high)).unwrap();
        assert!(new_high > high);
        assert_eq!(
            events.len(),
            1,
            "a new incarnation of elicitation-1 alerts exactly once"
        );
        sessions::save_session_to(&path, &crate::database::tests::session("parent", "project"))
            .unwrap();
        db.execute("DELETE FROM sessions WHERE session_id='parent'", [])
            .unwrap();
        let (after_destroy, remaining) = alert_events_since_with(&db, Some(0)).unwrap();
        assert_eq!(
            after_destroy, new_high,
            "sqlite_sequence preserves the durable frontier after deleting the last event"
        );
        assert!(
            remaining.is_empty(),
            "destroyed session titles and text cannot replay"
        );
    }

    // Hard-won: c21e3b62b: delivery admitted a paused holder before observer reconciliation.
    #[test]
    fn push_orders_one_device_tag_across_retry_and_worker_unavailability() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("push-test.sqlite3");
        let mut db = open(&path).unwrap();
        push_subscribe_with(&db, subscription()).unwrap();
        sessions::save_session_to(&path, &crate::database::tests::session("s", "project")).unwrap();
        let mut transcript = MaterializedSession::empty("s");
        transcript.applied_event_ordinal = 20;
        transcript.applied_event_digest = format!("{:064x}", 20);
        add_message(&mut transcript, "agent:first", 2, "First reply");
        add_message(&mut transcript, "agent:second", 9, "Second reply");
        add_message(&mut transcript, "agent:newer", 19, "Too new for both cards");
        materialized::save_materialized_session_to(&path, &transcript).unwrap();
        save_turn_history(&db, "s", &finished_turn(1, 3));
        save_turn_history(&db, "s", &finished_turn(8, 10));
        let mut runtime = runtime(false);
        assert!(claim(&mut db, &runtime, 0).is_empty()); // Cutover.
        show(
            &mut runtime,
            "s",
            vec![
                completion(3, EntryMessage::None),
                completion(10, EntryMessage::None),
            ],
        );
        let card = runtime
            .metadata
            .notification_cards
            .as_mut()
            .unwrap()
            .get_mut("s")
            .unwrap();
        card.holder = Some("child".into());
        card.form = Some(CardForm {
            raised_at_ms: 0,
            request_id: "form:1".into(),
            message: "Accept changes?".into(),
        });
        assert!(claim(&mut db, &runtime, 0).is_empty()); // Presence.
        runtime.metadata.operator_presence.away = true;
        let mut ahead = runtime.clone();
        ahead.metadata.notification_cards.as_mut().unwrap().clear();
        let (deferred, next) = push_reconcile_with(&mut db, &ahead, 0, false).unwrap();
        assert!(deferred.is_empty());
        assert!(
            next.is_none(),
            "a delivery snapshot ahead of the observer waits for reconciliation, not an expired timer"
        );
        let first = claim(&mut db, &runtime, 0);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].identity, r#"["completion","s",3]"#);
        assert_eq!(
            serde_json::from_str::<Notice>(&first[0].payload)
                .unwrap()
                .body,
            "First reply"
        );
        assert!(claim(&mut db, &runtime, 0).is_empty());
        result(&db, &first[0], Some(429), 10);
        runtime
            .metadata
            .session_attention
            .insert("child".into(), SessionAttention::Unreachable);
        let retry = claim(&mut db, &runtime, 10);
        assert_eq!(
            retry[0].identity, first[0].identity,
            "stack entry delivery is independent of the form holder"
        );
        runtime.metadata.session_attention.remove("child");
        assert!(claim(&mut db, &runtime, 9).is_empty());
        result(&db, &retry[0], Some(201), 10);

        let second = claim(&mut db, &runtime, 10);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].identity, r#"["completion","s",10]"#);
        assert_eq!(
            serde_json::from_str::<Notice>(&second[0].payload)
                .unwrap()
                .body,
            "Second reply"
        );
        result(&db, &second[0], Some(413), i64::MAX); // Permanently retires only this intent.
        runtime
            .metadata
            .session_attention
            .insert("child".into(), SessionAttention::Unreachable);
        let (paused, next) = push_reconcile_with(&mut db, &runtime, 10, false).unwrap();
        assert!(
            paused.is_empty(),
            "a paused holder cannot consume a still-visible ledger entry"
        );
        assert!(
            next.is_none(),
            "availability recovery wakes deferred delivery"
        );
        assert!(
            db.query_row(
                "SELECT visible FROM push_entries WHERE identity=?1",
                [r#"["form","child","form:1",0]"#],
                |row| row.get::<_, bool>(0)
            )
            .unwrap(),
            "the regression must exercise visibility ahead of reconciliation"
        );
        runtime.metadata.session_attention.remove("child");
        let form = claim(&mut db, &runtime, 10);
        assert_eq!(form.len(), 1);
        assert_eq!(form[0].identity, r#"["form","child","form:1",0]"#);
        let notice: Notice = serde_json::from_str(&form[0].payload).unwrap();
        assert_eq!(notice.body, "Accept changes?");
        assert_eq!(notice.url, "/#conversation/child");
        assert_eq!(notice.tag, "s");
        let cards = runtime.metadata.notification_cards.as_mut().unwrap();
        let moved = cards.remove("s").unwrap();
        cards.insert(
            "other".into(),
            NotificationCard {
                entries: vec![],
                ..moved
            },
        );
        show(&mut runtime, "s", vec![completion(20, EntryMessage::None)]);
        assert!(claim(&mut db, &runtime, 10).is_empty());
        result(&db, &form[0], Some(201), 10);
        let after_form = claim(&mut db, &runtime, 10);
        assert_eq!(after_form.len(), 1);
        assert_eq!(after_form[0].identity, r#"["completion","s",20]"#);
    }

    #[test]
    fn push_compatible_migration_baselines_first_authoritative_map() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("push-test.sqlite3");
        let mut db = open(&path).unwrap();
        let subscription = subscription();
        push_subscribe_with(&db, subscription.clone()).unwrap();
        db.execute_batch(
            "DROP TABLE push_entry_deliveries;
             DROP TABLE push_entries;
             DROP TABLE push_entry_cutover;
             DROP TABLE alert_events;
             DELETE FROM schema_migrations WHERE version>=138;
             PRAGMA user_version=137;
             INSERT INTO push_episodes VALUES('s','legacy','{}','[]',1);",
        )
        .unwrap();
        db.execute(
            "INSERT INTO push_deliveries(endpoint,episode,payload,accepted) VALUES(?1,'legacy','{}',0)",
            [&subscription.endpoint],
        ).unwrap();
        schema::migrate_schema(&db).unwrap();
        let schema = schema::read_schema_state(&db).unwrap();
        assert_eq!(schema.revision, super::super::SCHEMA_VERSION);
        schema.ensure_supported_by(137).unwrap();

        let mut runtime = runtime(true);
        runtime.metadata.notification_cards = None;
        assert!(claim(&mut db, &runtime, 0).is_empty());
        runtime.metadata.notification_cards = Some(Default::default());
        turn_only(&mut runtime, &["s"]);
        assert!(claim(&mut db, &runtime, 0).is_empty());
        assert!(
            db.query_row(
                "SELECT baseline FROM push_entries WHERE session='s'",
                [],
                |r| r.get::<_, bool>(0),
            )
            .unwrap()
        );
        drop(db);
        let mut db = open(&path).unwrap();
        turn_only(&mut runtime, &[]);
        assert!(claim(&mut db, &runtime, 0).is_empty());
        turn_only(&mut runtime, &["s", "new"]);
        let due = claim(&mut db, &runtime, 0);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].identity, r#"["completion","new",1]"#);
        assert_eq!(db.query_row(
            "SELECT q.accepted FROM push_deliveries q JOIN push_episodes e ON e.identity=q.episode WHERE q.episode='legacy'",
            [], |r| r.get::<_, u32>(0),
        ).unwrap(), 0);
    }

    // Hard-won: c146a8a89: worker recovery must retain the pending question's identity.
    #[test]
    fn push_restart_receipts_enrollment_and_expiry_are_durable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("push-test.sqlite3");
        let mut db = open(&path).unwrap();
        let subscription = subscription();
        push_subscribe_with(&db, subscription.clone()).unwrap();
        let mut runtime = runtime(true);
        assert!(claim(&mut db, &runtime, 0).is_empty());
        turn_only(&mut runtime, &["a", "b"]);
        let due = claim(&mut db, &runtime, 0);
        assert_eq!(due.len(), 2);
        assert!(claim(&mut db, &runtime, 0).is_empty());
        result(&db, &due[0], Some(201), 0);
        // Only the uncertain request replays across restart.
        drop(db);
        let mut db = open(&path).unwrap();
        push_recover_with(&db).unwrap();
        let retry = claim(&mut db, &runtime, 0);
        assert_eq!(retry.len(), 1);
        assert_eq!(retry[0].identity, due[1].identity);
        result(&db, &retry[0], Some(201), 0);

        // Backpressure carries across a routine browser refresh and applies to
        // new intents too.
        turn_only(&mut runtime, &["c"]);
        let pending = claim(&mut db, &runtime, 0).remove(0);
        result(&db, &pending, Some(429), 100);
        push_subscribe_with(&db, subscription.clone()).unwrap();
        turn_only(&mut runtime, &["c", "d"]);
        assert!(claim(&mut db, &runtime, 99).is_empty());
        let ready = claim(&mut db, &runtime, 100);
        assert_eq!(ready.len(), 2);
        for delivery in &ready {
            result(&db, delivery, Some(413), i64::MAX);
        }
        push_test_with(&mut db, subscription.endpoint.clone()).unwrap();
        let test = claim(&mut db, &RuntimeProjection::default(), 100).remove(0);
        result(&db, &test, Some(201), 0);

        // Expiry refresh preserves the flight, encryption replacement and ABA
        // invalidate old results, and endpoint rotation recovers uncertainty.
        turn_only(&mut runtime, &["e"]);
        let old = claim(&mut db, &runtime, 100).remove(0);
        let mut refreshed = subscription.clone();
        refreshed.expiration_time = Some(123.0);
        push_subscribe_with(&db, refreshed).unwrap();
        assert!(claim(&mut db, &runtime, 100).is_empty());
        let mut replaced = subscription.clone();
        replaced.keys.auth = "replacement".into();
        push_subscribe_with(&db, replaced).unwrap();
        let replacement = claim(&mut db, &runtime, 100).remove(0);
        result(&db, &old, Some(410), 0);
        push_subscribe_with(&db, subscription.clone()).unwrap();
        let aba = claim(&mut db, &runtime, 100).remove(0);
        assert_ne!(aba.enrollment, old.enrollment);
        result(&db, &replacement, Some(410), 0);
        result(&db, &old, Some(201), 0);
        result(&db, &aba, Some(201), 0);

        turn_only(&mut runtime, &["e", "f"]);
        let uncertain = claim(&mut db, &runtime, 100).remove(0);
        let mut rotated = subscription.clone();
        rotated.endpoint = "https://push.example.com/rotated".into();
        push_renew_with(&db, &subscription.endpoint, rotated.clone()).unwrap();
        push_renew_with(&db, &subscription.endpoint, rotated).unwrap();
        result(&db, &uncertain, Some(410), 0);
        let rotated_due = claim(&mut db, &runtime, 100);
        assert_eq!(rotated_due.len(), 1);
        assert_eq!(rotated_due[0].identity, uncertain.identity);
        result(&db, &rotated_due[0], Some(410), 0);
        assert!(claim(&mut db, &runtime, 100).is_empty());
    }
}

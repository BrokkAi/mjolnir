//! Durable Web Push reservations and receipts, owned by the dispatch service.
use super::*;
use crate::web_push::{Notice, Subscription};

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
                "UPDATE push_deliveries SET accepted=0,retry_at=0,failures=0,error=NULL WHERE endpoint=?1 AND accepted IN (0,3)",
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
    tx.execute("INSERT INTO push_deliveries(endpoint,episode,payload,accepted,retry_at,failures,error)
        SELECT ?2,q.episode,q.payload,CASE WHEN q.accepted=3 THEN 0 ELSE q.accepted END,q.retry_at,q.failures,q.error
        FROM push_deliveries q WHERE q.endpoint=?1
        ON CONFLICT(endpoint,episode) DO UPDATE SET accepted=CASE WHEN push_deliveries.accepted=3 THEN 3 WHEN push_deliveries.accepted=1 OR excluded.accepted=1 THEN 1 ELSE push_deliveries.accepted END",params![old_endpoint,subscription.endpoint])?;
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

pub(crate) fn push_observe(
    presence: OperatorPresence,
    episodes: Vec<(String, String, Notice)>,
    unavailable: Vec<String>,
) -> Result<()> {
    submit_database_write("reserve Web Push episodes", move |db| {
        push_observe_at_with(db, presence, episodes, unavailable)
    })
}
fn push_observe_at_with(
    db: &mut Connection,
    presence: OperatorPresence,
    episodes: Vec<(String, String, Notice)>,
    unavailable: Vec<String>,
) -> Result<()> {
    let away = presence.away;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute("INSERT INTO push_observation VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET away=excluded.away",[away])?;
    let previous_episodes = tx
        .prepare("SELECT session,identity,signature,payload FROM push_episodes")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ),
            ))
        })?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
    tx.execute("DELETE FROM push_episodes", [])?;
    // A disconnected or still-recovering worker supplies no evidence that its
    // question was answered. Retain episode identity but pause its sends.
    for session in unavailable {
        if let Some((identity, signature, payload)) = previous_episodes.get(&session) {
            tx.execute(
                "INSERT INTO push_episodes VALUES(?1,?2,?3,?4,0)",
                params![session, identity, payload, signature],
            )?;
        }
    }
    let mut reserved_episodes = Vec::new();
    for (session, signature, mut payload) in episodes {
        let identity = match previous_episodes.get(&session) {
            Some((identity, previous, _)) if same_episode(previous, &signature)? => {
                identity.clone()
            }
            _ => {
                let sequence:i64=tx.query_row("INSERT INTO push_episode_counters VALUES(?1,1) ON CONFLICT(session) DO UPDATE SET sequence=sequence+1 RETURNING sequence",[&session],|r|r.get(0))?;
                format!("{session}:{sequence}")
            }
        };
        payload.episode = Some(identity.clone());
        tx.execute(
            "INSERT INTO push_episodes VALUES(?1,?2,?3,?4,1)",
            params![
                session,
                identity,
                serde_json::to_string(&payload)?,
                signature
            ],
        )?;
        reserved_episodes.push((session, identity, payload));
    }
    let episodes = reserved_episodes;
    if away {
        let devices = tx
            .prepare("SELECT endpoint FROM push_devices")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for endpoint in devices {
            for (_, identity, notice) in &episodes {
                reserve(&tx, &endpoint, identity, notice)?;
            }
        }
    }
    tx.execute("DELETE FROM push_deliveries WHERE accepted=0 AND episode NOT LIKE 'test:%' AND episode NOT IN (SELECT identity FROM push_episodes)", [])?;
    tx.commit()?;
    Ok(())
}
#[cfg(test)]
pub(crate) fn same_episode_for_tests(previous: &str, next: &str) -> Result<bool> {
    same_episode(previous, next)
}
fn same_episode(previous: &str, next: &str) -> Result<bool> {
    let (old_kind, old): (String, Vec<String>) =
        serde_json::from_str(previous).context("read persisted push episode sources")?;
    let (kind, new): (String, Vec<String>) =
        serde_json::from_str(next).context("read published push episode sources")?;
    Ok(old_kind == kind && new.iter().all(|source| old.contains(source)))
}
fn reserve(tx: &Transaction<'_>, endpoint: &str, episode: &str, notice: &Notice) -> Result<()> {
    // One durable state: 0 queued, 1 accepted, 3 in flight.
    tx.execute("INSERT OR IGNORE INTO push_deliveries(endpoint,episode,payload,accepted) VALUES(?1,?2,?3,0)",params![endpoint,episode,serde_json::to_string(notice)?])?;
    Ok(())
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
    reserve(
        &tx,
        &endpoint,
        &format!(
            "test:{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ),
        &Notice {
            title: "Mjolnir".into(),
            body: "Notifications are enabled on this device".into(),
            session_id: None,
            url: "/".into(),
            tag: "test".into(),
            episode: None,
            observed_revision: 0,
        },
    )?;
    tx.commit()?;
    Ok(())
}
#[derive(Clone)]
pub(crate) struct PushDelivery {
    pub subscription: Subscription,
    pub enrollment: String,
    pub episode: String,
    pub payload: String,
    pub failures: u32,
}
// Admission and scheduling consume the same durable readiness rule. The
// database writer alone owns per-device/tag flight admission.
const READY_PUSH: &str = "FROM push_deliveries q JOIN push_devices d USING(endpoint)
 WHERE q.accepted=0 AND (q.episode LIKE 'test:%' OR ((SELECT away FROM push_observation WHERE singleton=1)=1
 AND q.episode IN (SELECT identity FROM push_episodes WHERE eligible=1)))
 AND NOT EXISTS(SELECT 1 FROM push_deliveries flight WHERE flight.endpoint=q.endpoint AND flight.accepted=3 AND json_extract(flight.payload,'$.tag')=json_extract(q.payload,'$.tag'))";
fn push_due_with(db: &Connection, now: i64) -> Result<Vec<PushDelivery>> {
    let sql = format!(
        "SELECT d.subscription,q.episode,q.payload,q.failures,d.enrollment {READY_PUSH} AND q.retry_at<=?1 AND d.retry_at<=?1"
    );
    db.prepare(&sql)?
        .query_map([now], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(subscription, episode, payload, failures, enrollment)| {
            Ok(PushDelivery {
                subscription: serde_json::from_str(&subscription)?,
                enrollment,
                episode,
                payload,
                failures,
            })
        })
        .collect()
}
pub(crate) fn push_claim(now: i64) -> Result<Vec<PushDelivery>> {
    submit_database_write("admit Web Push sends", move |db| push_claim_with(db, now))
}
fn push_claim_with(db: &Connection, now: i64) -> Result<Vec<PushDelivery>> {
    let tx = db.unchecked_transaction()?;
    let due = push_due_with(&tx, now)?;
    let mut admitted = Vec::new();
    for delivery in due {
        let changed=tx.execute("UPDATE push_deliveries AS q SET accepted=3 WHERE endpoint=?1 AND episode=?2 AND accepted=0
            AND NOT EXISTS(SELECT 1 FROM push_deliveries flight WHERE flight.endpoint=q.endpoint AND flight.accepted=3 AND json_extract(flight.payload,'$.tag')=json_extract(q.payload,'$.tag'))",params![delivery.subscription.endpoint,delivery.episode])?;
        if changed == 1 {
            admitted.push(delivery);
        }
    }
    tx.commit()?;
    Ok(admitted)
}
pub(crate) fn push_recover() -> Result<()> {
    submit_database_write("recover uncertain Web Push sends", |db| {
        push_recover_with(db)
    })
}
fn push_recover_with(db: &Connection) -> Result<()> {
    // Only service startup, under the daemon's store ownership, recovers flight
    // reservations. Remote acceptance may be unknown; receipts never reset.
    db.execute("UPDATE push_deliveries SET accepted=0 WHERE accepted=3", [])?;
    Ok(())
}
pub(crate) fn push_result(
    subscription: Subscription,
    enrollment: String,
    episode: String,
    status: Option<u16>,
    retry_at: i64,
    error: Option<String>,
) -> Result<()> {
    submit_database_write("record Web Push receipt", move |db| {
        apply_push_result_with(
            db,
            subscription,
            &enrollment,
            episode,
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
    episode: String,
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
    let owns_flight:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM push_deliveries WHERE endpoint=?1 AND episode=?2 AND accepted=3)",params![subscription.endpoint,episode],|r|r.get(0))?;
    if !owns_flight {
        return Ok(());
    }
    // A permanent payload rejection retires only that intent. Endpoint-wide
    // backpressure is reserved for transient service/transport failures.
    if status.is_none() || status.is_some_and(|status| status == 429 || status >= 500) {
        // Retry-After constrains follow-up requests to this endpoint, including
        // newly queued episodes, not just the rejected intent.
        tx.execute(
            "UPDATE push_devices SET retry_at=max(retry_at,?2) WHERE endpoint=?1",
            params![subscription.endpoint, retry_at],
        )?;
    }
    push_result_with(
        &tx,
        subscription.endpoint,
        episode,
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
    episode: String,
    accepted: bool,
    retry_at: i64,
    error: Option<String>,
) -> Result<()> {
    db.execute("UPDATE push_deliveries SET accepted=?3,retry_at=?4,failures=failures+1,error=?5 WHERE endpoint=?1 AND episode=?2 AND accepted IN (0,3)",params![endpoint,episode,accepted,retry_at,error])?;
    Ok(())
}
pub(crate) fn push_next_due() -> Result<Option<i64>> {
    let db = open_reader(&database_path())?;
    Ok(db.query_row(
        &format!("SELECT min(max(q.retry_at,d.retry_at)) {READY_PUSH}"),
        [],
        |r| r.get(0),
    )?)
}
pub(crate) fn push_device_status(endpoint: String) -> Result<serde_json::Value> {
    let db = open_reader(&database_path())?;
    let subscribed = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM push_devices WHERE endpoint=?1)",
        [&endpoint],
        |r| r.get::<_, bool>(0),
    )?;
    let error:Option<String>=db.query_row("SELECT error FROM push_deliveries WHERE endpoint=?1 AND accepted IN (0,3) AND error IS NOT NULL ORDER BY retry_at DESC LIMIT 1",[endpoint],|r|r.get(0)).optional()?.flatten();
    Ok(serde_json::json!({"subscribed":subscribed,"error":error}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn episode(id: &str) -> (String, String, Notice) {
        (
            id.into(),
            serde_json::to_string(&("needs_input", vec![format!("{id}:form:unique")])).unwrap(),
            Notice {
                title: id.into(),
                body: "Which option?".into(),
                session_id: Some(id.into()),
                url: format!("/#conversation/{id}"),
                tag: id.into(),
                episode: None,
                observed_revision: 40,
            },
        )
    }
    fn observe(db: &mut Connection, away: bool, ids: &[&str]) {
        push_observe_at_with(
            db,
            OperatorPresence {
                away,
                revision: u64::from(away),
            },
            ids.iter().map(|id| episode(id)).collect(),
            vec![],
        )
        .unwrap();
    }
    fn result(db: &Connection, delivery: &PushDelivery, status: Option<u16>, retry_at: i64) {
        apply_push_result_with(
            db,
            delivery.subscription.clone(),
            &delivery.enrollment,
            delivery.episode.clone(),
            status,
            retry_at,
            None,
        )
        .unwrap();
    }

    // Hard-won: c146a8a89: worker recovery must retain the pending question's identity.
    #[test]
    fn push_restart_receipts_presence_and_expiry_are_durable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("push-test.sqlite3");
        let mut db = open(&path).unwrap();
        let subscription = Subscription {
            endpoint: "https://push.example.com/device".into(),
            keys: crate::web_push::SubscriptionKeys {
                p256dh: "key".into(),
                auth: "auth".into(),
            },
            expiration_time: None,
        };
        push_subscribe_with(&db, subscription.clone()).unwrap();
        observe(&mut db, false, &["a", "b"]);
        assert!(push_claim_with(&db, 0).unwrap().is_empty());
        observe(&mut db, true, &["a", "b"]);
        let due = push_claim_with(&db, 0).unwrap();
        for delivery in &due {
            let notice: Notice = serde_json::from_str(&delivery.payload).unwrap();
            assert_eq!(notice.episode.as_deref(), Some(delivery.episode.as_str()));
            assert_eq!(notice.observed_revision, 40);
        }
        assert_eq!(due.len(), 2); // Turning away sends individual open episodes.
        assert!(due.iter().all(|q| {
            serde_json::from_str::<Notice>(&q.payload)
                .unwrap()
                .session_id
                .is_some()
        }));
        assert!(push_claim_with(&db, 0).unwrap().is_empty());
        result(&db, &due[0], Some(201), 0);
        // Crash after remote acceptance but before receipt: only the uncertain
        // request replays; the acknowledged request never resets.
        drop(db);
        let mut db = open(&path).unwrap();
        push_recover_with(&db).unwrap();
        observe(&mut db, true, &["a", "b"]);
        let retry = push_claim_with(&db, 0).unwrap();
        assert_eq!(retry.len(), 1);
        assert_eq!(retry[0].episode, due[1].episode);
        result(&db, &retry[0], Some(201), 0);
        observe(&mut db, false, &["a", "b"]);
        observe(&mut db, true, &["a", "b"]);
        assert!(push_claim_with(&db, 0).unwrap().is_empty());

        // Temporary missing attention pauses, rather than clears, the episode.
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: true,
                revision: 3,
            },
            vec![],
            vec!["a".into(), "b".into()],
        )
        .unwrap();
        observe(&mut db, true, &["a", "b"]);
        assert!(push_claim_with(&db, 0).unwrap().is_empty());
        // A newly promoted child question starts a new episode; removing an
        // answered source keeps that episode and its receipt.
        let mut question = episode("a");
        question.1 =
            serde_json::to_string(&("needs_input", vec!["a:form:unique", "child:form:new"]))
                .unwrap();
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: true,
                revision: 3,
            },
            vec![question],
            vec![],
        )
        .unwrap();
        let child = push_claim_with(&db, 0).unwrap().remove(0);
        result(&db, &child, Some(201), 0);
        observe(&mut db, true, &["a"]);
        assert!(push_claim_with(&db, 0).unwrap().is_empty());
        observe(&mut db, true, &[]);
        observe(&mut db, true, &["a"]);
        let reentry = push_claim_with(&db, 0).unwrap().remove(0);
        assert_ne!(reentry.episode, child.episode);
        result(&db, &reentry, Some(429), 100);
        observe(&mut db, true, &["a", "c"]);
        push_subscribe_with(&db, subscription.clone()).unwrap();
        // Hard-won: routine browser reconciliation reset pending delivery state.
        assert_eq!(
            db.query_row(
                "SELECT retry_at FROM push_deliveries WHERE endpoint=?1 AND episode=?2",
                params![subscription.endpoint, reentry.episode],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            100
        );
        assert!(push_claim_with(&db, 99).unwrap().is_empty());
        let after_backpressure = push_claim_with(&db, 100).unwrap();
        assert_eq!(after_backpressure.len(), 2);
        for q in &after_backpressure {
            result(&db, q, Some(413), i64::MAX);
        }
        // A rejected payload does not disable a device or future tests.
        push_test_with(&mut db, subscription.endpoint.clone()).unwrap();
        let test = push_claim_with(&db, 100).unwrap().remove(0);
        result(&db, &test, Some(201), 0);

        // Hard-won enrollment races: refresh preserves ownership, key replacement
        // releases old reservations, and ABA never revives an old completion.
        observe(&mut db, true, &["d"]);
        let old = push_claim_with(&db, 100).unwrap().remove(0);
        let mut refreshed = subscription.clone();
        refreshed.expiration_time = Some(123.0);
        push_subscribe_with(&db, refreshed).unwrap();
        assert!(push_claim_with(&db, 100).unwrap().is_empty());
        let mut replaced = subscription.clone();
        replaced.keys.auth = "replacement".into();
        push_subscribe_with(&db, replaced).unwrap();
        let replacement = push_claim_with(&db, 100).unwrap().remove(0);
        result(&db, &old, Some(410), 0);
        push_subscribe_with(&db, subscription.clone()).unwrap();
        let aba = push_claim_with(&db, 100).unwrap().remove(0);
        assert_ne!(aba.enrollment, old.enrollment);
        result(&db, &replacement, Some(410), 0);
        result(&db, &old, Some(201), 0);
        result(&db, &aba, Some(201), 0);
        assert!(push_claim_with(&db, 100).unwrap().is_empty());

        // Native endpoint rotation preserves acceptance and recovers uncertain
        // work. A late result from the retired endpoint cannot delete its owner.
        observe(&mut db, true, &["d", "e"]);
        let uncertain = push_claim_with(&db, 100).unwrap().remove(0);
        let mut rotated = subscription.clone();
        rotated.endpoint = "https://push.example.com/rotated".into();
        push_renew_with(&db, &subscription.endpoint, rotated.clone()).unwrap();
        push_renew_with(&db, &subscription.endpoint, rotated.clone()).unwrap();
        result(&db, &uncertain, Some(410), 0);
        let rotated_due = push_claim_with(&db, 100).unwrap();
        assert_eq!(rotated_due.len(), 1);
        assert_eq!(rotated_due[0].episode, uncertain.episode);
        result(&db, &rotated_due[0], Some(410), 0);
        assert_eq!(
            db.query_row("SELECT count(*) FROM push_devices", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(push_claim_with(&db, 100).unwrap().is_empty());
    }
}

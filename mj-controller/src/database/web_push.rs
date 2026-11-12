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
    tx.execute("INSERT INTO push_devices(endpoint,subscription) VALUES (?1,?2) ON CONFLICT(endpoint) DO UPDATE SET subscription=excluded.subscription",
        params![subscription.endpoint, serde_json::to_string(&subscription)?])?;
    tx.execute("UPDATE push_deliveries SET retry_at=0,failures=0,error=NULL WHERE endpoint=?1 AND accepted=0",[subscription.endpoint])?;
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
    let previous: Option<(bool, u64)> = tx
        .query_row(
            "SELECT away,revision FROM push_observation WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let same_away_period = previous == Some((true, presence.revision));
    tx.execute("INSERT INTO push_observation VALUES(1,?1,?2) ON CONFLICT(singleton) DO UPDATE SET away=excluded.away,revision=excluded.revision",params![away,presence.revision])?;
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
    for (session, signature, payload) in episodes {
        let identity = match previous_episodes.get(&session) {
            Some((identity, previous, _)) if same_episode(previous, &signature) => identity.clone(),
            _ => {
                let sequence:i64=tx.query_row("INSERT INTO push_episode_counters VALUES(?1,1) ON CONFLICT(session) DO UPDATE SET sequence=sequence+1 RETURNING sequence",[&session],|r|r.get(0))?;
                format!("{session}:{sequence}")
            }
        };
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
            let mut unsent = Vec::new();
            for (_, identity, notice) in &episodes {
                let reserved: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM push_deliveries q WHERE endpoint=?1 AND episode=?2 AND (?3 OR accepted IN (1,3) OR (accepted=2 AND EXISTS(SELECT 1 FROM push_deliveries summary WHERE summary.endpoint=q.endpoint AND summary.episode=q.summary_episode AND summary.accepted IN (1,3)))))",
                    params![endpoint, identity, same_away_period],
                    |r| r.get(0),
                )?;
                if !reserved {
                    unsent.push((identity, notice));
                }
            }
            if !same_away_period && !unsent.is_empty() {
                let ids = unsent
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>()
                    .join("|");
                let summary_id = format!("summary:{ids}");
                let summary = Notice {
                    title: format!("{} sessions need you", unsent.len()),
                    body: "Open Mjolnir to see them".into(),
                    session_id: None,
                    url: crate::web_push::summary_url(&endpoint, &summary_id),
                    tag: "summary".into(),
                };
                tx.execute("DELETE FROM push_deliveries WHERE endpoint=?1 AND accepted=0 AND episode LIKE 'summary:%'",[&endpoint])?;
                let coverage = unsent
                    .iter()
                    .filter_map(|(_, notice)| notice.session_id.clone())
                    .collect::<Vec<_>>();
                // Immutable cleanup coverage outlives subscription expiry and
                // outbox retirement, including unknown remote acceptance.
                tx.execute(
                    "INSERT OR IGNORE INTO push_summary_coverage VALUES(?1,?2)",
                    params![summary.url, serde_json::to_string(&coverage)?],
                )?;
                reserve(&tx, &endpoint, &summary_id, &summary, 0)?;
                for (id, notice) in unsent {
                    reserve(&tx, &endpoint, id, notice, 2)?;
                    tx.execute("UPDATE push_deliveries SET accepted=2,summary_episode=?3 WHERE endpoint=?1 AND episode=?2",params![endpoint,id,summary_id])?;
                }
            } else {
                for (id, notice) in unsent {
                    reserve(&tx, &endpoint, id, notice, 0)?;
                }
            }
        }
    }
    // Coverage belongs to the outbox transaction. An unavailable episode keeps
    // its identity and becomes a paused individual intent; resolved ones retire.
    tx.execute("UPDATE push_deliveries AS covered SET accepted=0,summary_episode=NULL
        WHERE accepted=2 AND (NOT EXISTS(SELECT 1 FROM push_deliveries summary WHERE summary.endpoint=covered.endpoint AND summary.episode=covered.summary_episode)
        OR (EXISTS(SELECT 1 FROM push_deliveries summary WHERE summary.endpoint=covered.endpoint AND summary.episode=covered.summary_episode AND summary.accepted=0)
        AND episode NOT IN (SELECT identity FROM push_episodes WHERE eligible=1)))",[])?;
    tx.execute("DELETE FROM push_deliveries WHERE accepted=0 AND episode NOT LIKE 'summary:%' AND episode NOT LIKE 'test:%' AND episode NOT IN (SELECT identity FROM push_episodes)", [])?;
    let summaries=tx.prepare("SELECT endpoint,episode,payload FROM push_deliveries WHERE accepted=0 AND episode LIKE 'summary:%'")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (endpoint, episode, payload) in summaries {
        let covered=tx.prepare("SELECT e.session FROM push_deliveries covered JOIN push_episodes e ON e.identity=covered.episode WHERE covered.endpoint=?1 AND covered.summary_episode=?2 AND covered.accepted=2 AND e.eligible=1 ORDER BY e.session")?.query_map(params![endpoint,episode],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        if covered.is_empty() {
            tx.execute(
                "DELETE FROM push_deliveries WHERE endpoint=?1 AND episode=?2",
                params![endpoint, episode],
            )?;
        } else {
            let mut notice: Notice = serde_json::from_str(&payload)?;
            notice.title = format!("{} sessions need you", covered.len());
            tx.execute(
                "UPDATE push_deliveries SET payload=?3 WHERE endpoint=?1 AND episode=?2",
                params![endpoint, episode, serde_json::to_string(&notice)?],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}
fn same_episode(previous: &str, next: &str) -> bool {
    let parse = |value: &str| serde_json::from_str::<(String, Vec<String>)>(value);
    match (parse(previous), parse(next)) {
        (Ok((old_kind, old)), Ok((kind, new))) => {
            old_kind == kind && new.iter().all(|source| old.contains(source))
        }
        _ => previous == next,
    }
}
fn reserve(
    tx: &Transaction<'_>,
    endpoint: &str,
    episode: &str,
    notice: &Notice,
    accepted: i64,
) -> Result<()> {
    // One durable state: 0 queued, 1 accepted, 2 summary-covered, 3 in flight.
    tx.execute("INSERT OR IGNORE INTO push_deliveries(endpoint,episode,payload,accepted) VALUES(?1,?2,?3,?4)",params![endpoint,episode,serde_json::to_string(notice)?,accepted])?;
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
        },
        0,
    )?;
    tx.commit()?;
    Ok(())
}
#[derive(Clone)]
pub(crate) struct PushDelivery {
    pub subscription: Subscription,
    pub episode: String,
    pub payload: String,
    pub failures: u32,
}
// Admission and scheduling consume the same durable readiness rule. The
// database writer alone owns per-device/tag flight admission and summary cover.
const READY_PUSH: &str = "FROM push_deliveries q JOIN push_devices d USING(endpoint)
 WHERE q.accepted=0 AND (q.episode LIKE 'test:%' OR ((SELECT away FROM push_observation WHERE singleton=1)=1
 AND ((q.episode LIKE 'summary:%' AND EXISTS(SELECT 1 FROM push_episodes WHERE eligible=1))
 OR q.episode IN (SELECT identity FROM push_episodes WHERE eligible=1))))
 AND NOT EXISTS(SELECT 1 FROM push_deliveries flight WHERE flight.endpoint=q.endpoint AND flight.accepted=3 AND json_extract(flight.payload,'$.tag')=json_extract(q.payload,'$.tag'))";
fn push_due_with(db: &Connection, now: i64) -> Result<Vec<PushDelivery>> {
    let sql = format!(
        "SELECT d.subscription,q.episode,q.payload,q.failures {READY_PUSH} AND q.retry_at<=?1 AND d.retry_at<=?1"
    );
    db.prepare(&sql)?
        .query_map([now], |r| {
            Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(subscription, episode, payload, failures)| {
            Ok(PushDelivery {
                subscription: serde_json::from_str(&subscription)?,
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
    episode: String,
    status: Option<u16>,
    retry_at: i64,
    error: Option<String>,
) -> Result<()> {
    submit_database_write("record Web Push receipt", move |db| {
        apply_push_result_with(db, subscription, episode, status, retry_at, error)
    })
}
fn apply_push_result_with(
    db: &Connection,
    subscription: Subscription,
    episode: String,
    status: Option<u16>,
    retry_at: i64,
    error: Option<String>,
) -> Result<()> {
    let tx = db.unchecked_transaction()?;
    let current: Option<String> = tx
        .query_row(
            "SELECT subscription FROM push_devices WHERE endpoint=?1",
            [&subscription.endpoint],
            |r| r.get(0),
        )
        .optional()?;
    if current.as_deref() != Some(serde_json::to_string(&subscription)?.as_str()) {
        return Ok(());
    }
    if matches!(status, Some(404 | 410)) {
        // A response from an older enrollment cannot delete its replacement.
        tx.execute(
            "DELETE FROM push_devices WHERE endpoint=?1 AND subscription=?2",
            params![subscription.endpoint, serde_json::to_string(&subscription)?],
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
        // newly queued episodes and summaries, not just the rejected intent.
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
pub(crate) fn push_summary_coverage(id: String) -> Result<Option<Vec<String>>> {
    let db = open_reader(&database_path())?;
    push_summary_coverage_with(&db, &id)
}
fn push_summary_coverage_with(db: &Connection, id: &str) -> Result<Option<Vec<String>>> {
    let url = format!("/?push-summary={id}");
    let sessions: Option<String> = db
        .query_row(
            "SELECT sessions FROM push_summary_coverage WHERE url=?1",
            [url],
            |r| r.get(0),
        )
        .optional()?;
    sessions
        .map(|sessions| Ok(serde_json::from_str(&sessions)?))
        .transpose()
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
    fn push_observe_with(
        db: &mut Connection,
        away: bool,
        episodes: Vec<(String, String, Notice)>,
    ) -> Result<()> {
        super::push_observe_at_with(
            db,
            OperatorPresence {
                away,
                revision: u64::from(away),
            },
            episodes,
            vec![],
        )
    }
    fn episode(id: &str) -> (String, String, Notice) {
        (
            id.into(),
            format!("{id}:form:unique"),
            Notice {
                title: id.into(),
                body: "Which option?".into(),
                session_id: Some(id.into()),
                url: format!("/#conversation/{id}"),
                tag: id.into(),
            },
        )
    }
    #[test]
    fn push_restart_receipts_presence_summary_and_expiry_are_durable() {
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
        let endpoint = subscription.endpoint.clone();
        push_subscribe_with(&db, subscription.clone()).unwrap();
        push_observe_with(&mut db, false, vec![episode("a"), episode("b")]).unwrap();
        assert!(push_due_with(&db, 0).unwrap().is_empty());
        push_observe_with(&mut db, true, vec![episode("a"), episode("b")]).unwrap();
        let due = push_due_with(&db, 0).unwrap();
        assert_eq!(due.len(), 1);
        assert!(due[0].episode.starts_with("summary:"));
        assert_eq!(
            serde_json::from_str::<Notice>(&due[0].payload)
                .unwrap()
                .title,
            "2 sessions need you"
        );
        // Unknown outcomes remain queued across a process-equivalent reconnect.
        drop(db);
        let mut db = open(&path).unwrap();
        push_observe_with(&mut db, true, vec![episode("a"), episode("b")]).unwrap();
        assert_eq!(push_due_with(&db, 0).unwrap().len(), 1);
        push_result_with(&db, endpoint.clone(), due[0].episode.clone(), true, 0, None).unwrap();
        drop(db);
        let mut db = open(&path).unwrap();
        push_observe_with(&mut db, true, vec![episode("a"), episode("b")]).unwrap();
        assert!(push_due_with(&db, 0).unwrap().is_empty());
        push_observe_with(
            &mut db,
            true,
            vec![episode("a"), episode("b"), episode("c")],
        )
        .unwrap();
        let due = push_due_with(&db, 0).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].episode, "c:1");
        push_result_with(&db, endpoint.clone(), due[0].episode.clone(), true, 0, None).unwrap();
        drop(db);
        let mut db = open(&path).unwrap();
        push_observe_with(
            &mut db,
            true,
            vec![episode("a"), episode("b"), episode("c")],
        )
        .unwrap();
        assert!(push_due_with(&db, 0).unwrap().is_empty());
        // Restart recovery can briefly expose Unreachable. That pauses sends
        // without inventing a fresh episode when the same question reconnects.
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: true,
                revision: 1,
            },
            vec![],
            vec!["a".into(), "b".into(), "c".into()],
        )
        .unwrap();
        push_observe_with(
            &mut db,
            true,
            vec![episode("a"), episode("b"), episode("c")],
        )
        .unwrap();
        assert!(push_due_with(&db, 0).unwrap().is_empty());
        // A watch feed can coalesce present -> away. The owner's revision
        // still makes that a new away period and summarizes queued retries.
        push_observe_with(&mut db, true, vec![episode("x"), episode("y")]).unwrap();
        assert_eq!(push_due_with(&db, 0).unwrap().len(), 2);
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: true,
                revision: 3,
            },
            vec![episode("x"), episode("y")],
            vec![],
        )
        .unwrap();
        let summary = push_due_with(&db, 0).unwrap();
        assert_eq!(summary.len(), 1);
        assert!(summary[0].episode.starts_with("summary:"));
        push_result_with(
            &db,
            endpoint.clone(),
            summary[0].episode.clone(),
            true,
            0,
            None,
        )
        .unwrap();
        // Removing one answered form retains the episode. A newly added form,
        // or re-entry after all attention clears, allocates a new sequence.
        let mut forms = episode("forms");
        forms.1 = serde_json::to_string(&("needs_input", vec!["form:a", "form:b"])).unwrap();
        push_observe_with(&mut db, true, vec![forms.clone()]).unwrap();
        let due = push_due_with(&db, 0).unwrap();
        assert_eq!(due.len(), 1);
        push_result_with(&db, endpoint.clone(), due[0].episode.clone(), true, 0, None).unwrap();
        forms.1 = serde_json::to_string(&("needs_input", vec!["form:b"])).unwrap();
        push_observe_with(&mut db, true, vec![forms.clone()]).unwrap();
        assert!(push_due_with(&db, 0).unwrap().is_empty());
        forms.1 = serde_json::to_string(&("needs_input", vec!["form:b", "form:c"])).unwrap();
        push_observe_with(&mut db, true, vec![forms.clone()]).unwrap();
        assert_eq!(push_due_with(&db, 0).unwrap().len(), 1);
        push_observe_with(&mut db, true, vec![]).unwrap();
        push_observe_with(&mut db, true, vec![forms]).unwrap();
        assert_eq!(push_due_with(&db, 0).unwrap().len(), 1);
        // A real service refusal blocks follow-up requests to the device,
        // including a new episode, until the persisted service deadline.
        let pending = push_claim_with(&db, 0).unwrap();
        apply_push_result_with(
            &db,
            subscription.clone(),
            pending[0].episode.clone(),
            Some(429),
            100,
            Some("push service returned 429".into()),
        )
        .unwrap();
        push_observe_with(&mut db, true, vec![episode("new-after-refusal")]).unwrap();
        // Re-enrollment cannot erase a real endpoint Retry-After deadline.
        push_subscribe_with(&db, subscription.clone()).unwrap();
        assert!(push_due_with(&db, 99).unwrap().is_empty());
        drop(db);
        let mut db = open(&path).unwrap();
        assert_eq!(push_due_with(&db, 100).unwrap().len(), 1);
        // Hard-won: c146a8a89: unrelated attention kept obsolete summaries queued.
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: true,
                revision: 5,
            },
            vec![episode("summary-a"), episode("summary-b")],
            vec![],
        )
        .unwrap();
        let summary = push_due_with(&db, 100).unwrap();
        assert_eq!(summary.len(), 1);
        let notice: Notice = serde_json::from_str(&summary[0].payload).unwrap();
        let reference = notice.url.split("push-summary=").nth(1).unwrap().to_owned();
        assert_eq!(
            push_summary_coverage_with(&db, &reference)
                .unwrap()
                .unwrap(),
            vec!["summary-a", "summary-b"]
        );
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: true,
                revision: 5,
            },
            vec![episode("summary-b"), episode("unrelated")],
            vec![],
        )
        .unwrap();
        let pending = push_due_with(&db, 100).unwrap();
        let summary = pending
            .iter()
            .find(|item| item.episode.starts_with("summary:"))
            .unwrap();
        let notice: Notice = serde_json::from_str(&summary.payload).unwrap();
        assert_eq!(notice.title, "1 sessions need you");
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: true,
                revision: 5,
            },
            vec![episode("unrelated")],
            vec![],
        )
        .unwrap();
        let pending = push_due_with(&db, 100).unwrap();
        assert_eq!(pending.len(), 1);
        assert!(!pending[0].episode.starts_with("summary:"));
        // A phone can already show a summary whose remote acceptance was
        // unknown. Retirement must retain its lookup data for later cleanup.
        assert_eq!(
            push_summary_coverage_with(&db, &reference)
                .unwrap()
                .unwrap(),
            vec!["summary-a", "summary-b"]
        );
        // Hard-won: 4c4eefc4c: a permanently rejected summary disabled its device.
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: true,
                revision: 7,
            },
            vec![episode("reject-a"), episode("reject-b")],
            vec![],
        )
        .unwrap();
        let rejected = push_claim_with(&db, 100).unwrap();
        assert_eq!(rejected.len(), 1);
        apply_push_result_with(
            &db,
            subscription.clone(),
            rejected[0].episode.clone(),
            Some(413),
            i64::MAX,
            Some("payload rejected".into()),
        )
        .unwrap();
        push_test_with(&mut db, endpoint.clone()).unwrap();
        let test = push_claim_with(&db, 100).unwrap();
        assert_eq!(test.len(), 1);
        assert!(test[0].episode.starts_with("test:"));
        apply_push_result_with(
            &db,
            subscription.clone(),
            test[0].episode.clone(),
            Some(201),
            0,
            None,
        )
        .unwrap();
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: true,
                revision: 7,
            },
            vec![
                episode("reject-a"),
                episode("reject-b"),
                episode("after-rejection"),
            ],
            vec![],
        )
        .unwrap();
        let later = push_claim_with(&db, 100).unwrap();
        assert_eq!(later.len(), 1);
        assert!(later[0].episode.starts_with("after-rejection:"));
        apply_push_result_with(
            &db,
            subscription.clone(),
            later[0].episode.clone(),
            Some(201),
            0,
            None,
        )
        .unwrap();
        // Hard-won: 4c4eefc4c: an in-flight individual could also be summarized.
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: true,
                revision: 7,
            },
            vec![episode("flight")],
            vec![],
        )
        .unwrap();
        let flight = push_claim_with(&db, 100).unwrap();
        assert_eq!(flight.len(), 1);
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: false,
                revision: 8,
            },
            vec![episode("flight")],
            vec![],
        )
        .unwrap();
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: true,
                revision: 9,
            },
            vec![episode("flight")],
            vec![],
        )
        .unwrap();
        assert!(push_claim_with(&db, 100).unwrap().is_empty());
        apply_push_result_with(
            &db,
            subscription.clone(),
            flight[0].episode.clone(),
            Some(503),
            101,
            Some("retry".into()),
        )
        .unwrap();
        let retry = push_claim_with(&db, 101).unwrap();
        assert_eq!(retry.len(), 1);
        assert_eq!(retry[0].episode, flight[0].episode);
        // Recovery clears flight ownership, never an acceptance receipt.
        drop(db);
        let mut db = open(&path).unwrap();
        push_recover_with(&db).unwrap();
        let retry = push_claim_with(&db, 101).unwrap();
        assert_eq!(retry.len(), 1);
        apply_push_result_with(
            &db,
            subscription.clone(),
            retry[0].episode.clone(),
            Some(201),
            0,
            None,
        )
        .unwrap();
        push_recover_with(&db).unwrap();
        assert!(push_claim_with(&db, 101).unwrap().is_empty());
        // An in-flight summary likewise keeps ownership of its covered episodes.
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: true,
                revision: 11,
            },
            vec![episode("summary-flight")],
            vec![],
        )
        .unwrap();
        let flight = push_claim_with(&db, 101).unwrap();
        assert_eq!(flight.len(), 1);
        assert!(flight[0].episode.starts_with("summary:"));
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: false,
                revision: 12,
            },
            vec![episode("summary-flight")],
            vec![],
        )
        .unwrap();
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: true,
                revision: 13,
            },
            vec![episode("summary-flight")],
            vec![],
        )
        .unwrap();
        assert!(push_claim_with(&db, 101).unwrap().is_empty());
        apply_push_result_with(
            &db,
            subscription.clone(),
            flight[0].episode.clone(),
            Some(201),
            0,
            None,
        )
        .unwrap();
        // A stale completion cannot turn summary-covered work back into a retry.
        apply_push_result_with(
            &db,
            subscription.clone(),
            "summary-flight:1".into(),
            Some(503),
            102,
            Some("stale".into()),
        )
        .unwrap();
        assert!(push_claim_with(&db, 102).unwrap().is_empty());
        // Hard-won: 4c4eefc4c: summary coverage exceeded the push payload limit.
        let many = (0..1000)
            .map(|n| episode(&format!("large-{n:032x}")))
            .collect();
        push_observe_at_with(
            &mut db,
            OperatorPresence {
                away: true,
                revision: 15,
            },
            many,
            vec![],
        )
        .unwrap();
        let large = push_claim_with(&db, 103).unwrap();
        assert_eq!(large.len(), 1);
        // RFC 8291 section 4 documents 3993 available plaintext bytes.
        assert!(large[0].payload.len() <= 3993);
        let notice: Notice = serde_json::from_str(&large[0].payload).unwrap();
        assert_eq!(notice.title, "1000 sessions need you");
        let reference = notice.url.split("push-summary=").nth(1).unwrap();
        let coverage = push_summary_coverage_with(&db, reference).unwrap().unwrap();
        assert_eq!(coverage.len(), 1000);
        assert!(serde_json::to_string(&coverage).unwrap().len() > 4096);
        // HTTP 410 cascades queued intents and receipts.

        apply_push_result_with(&db, subscription.clone(), String::new(), Some(410), 0, None)
            .unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM push_deliveries", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        // Coverage is owned independently of a subscription's lifetime, so a
        // shown summary can still be withdrawn after the endpoint expires.
        assert_eq!(
            push_summary_coverage_with(&db, reference)
                .unwrap()
                .unwrap()
                .len(),
            1000
        );
    }
}

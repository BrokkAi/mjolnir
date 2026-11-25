use super::*;
use mj_client::quota::ProfileQuota;

pub fn load_quota_cache(identity: &str) -> Result<Option<ProfileQuota>> {
    load(&*open_reader(&database_path())?, identity)
}

fn load(connection: &Connection, identity: &str) -> Result<Option<ProfileQuota>> {
    let body: Option<String> = connection
        .query_row(
            "SELECT body FROM quota_reset_cache WHERE identity = ?1",
            [identity],
            |row| row.get(0),
        )
        .optional()?;
    body.map(|body| serde_json::from_str(&body).context("decode cached quota resets"))
        .transpose()
}

/// The probes saved for one configuration of a profile at or after `since`,
/// newest first, failed ones included.
pub fn load_quota_history_since(
    identity: &str,
    profile_id: &str,
    since: u64,
) -> Result<Vec<ProfileQuota>> {
    history_since(
        &*open_reader(&database_path())?,
        identity,
        profile_id,
        since,
    )
}

fn history_since(
    connection: &Connection,
    identity: &str,
    profile_id: &str,
    since: u64,
) -> Result<Vec<ProfileQuota>> {
    let bodies = connection
        .prepare(
            "SELECT body FROM quota_history
             WHERE profile_id = ?1 AND sampled_at_epoch_seconds >= ?2 AND identity = ?3
             ORDER BY sampled_at_epoch_seconds DESC, id DESC",
        )?
        .query_map(params![profile_id, since, identity], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    bodies
        .iter()
        .map(|body| serde_json::from_str(body).context("decode stored quota probe"))
        .collect()
}

/// The probes that measure consumption over `since..=until`, oldest first: the
/// nearest usable probe before `since`, every probe inside the span (failed
/// ones included), and the nearest usable probe after `until`. A usable probe
/// has no error and at least one window; a failed probe carries no reading, so
/// it cannot stand for the state at the span's edge. Each edge is one
/// `LIMIT 1` walk of `quota_history_profile_time`, so no lookback is assumed
/// and no history outside the span is loaded. An edge with no usable probe is
/// absent, and the caller treats the span as not bracketed there.
pub fn load_quota_history_bracketing(
    identity: &str,
    profile_id: &str,
    since: u64,
    until: u64,
) -> Result<Vec<ProfileQuota>> {
    history_bracketing(
        &*open_reader(&database_path())?,
        identity,
        profile_id,
        since,
        until,
    )
}

fn history_bracketing(
    connection: &Connection,
    identity: &str,
    profile_id: &str,
    since: u64,
    until: u64,
) -> Result<Vec<ProfileQuota>> {
    const USABLE: &str = "json_extract(body, '$.error') IS NULL
         AND json_array_length(body, '$.windows') > 0";
    let query = |filter: &str, order: &str, limit: &str, bounds: &[u64]| -> Result<Vec<String>> {
        let sql = format!(
            "SELECT body FROM quota_history
             WHERE profile_id = ?1 AND identity = ?2 AND {filter}
             ORDER BY sampled_at_epoch_seconds {order}, id {order} {limit}"
        );
        let mut values: Vec<&dyn rusqlite::ToSql> = vec![&profile_id, &identity];
        values.extend(bounds.iter().map(|bound| bound as &dyn rusqlite::ToSql));
        connection
            .prepare(&sql)?
            .query_map(values.as_slice(), |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(anyhow::Error::from)
    };
    let before = query(
        &format!("sampled_at_epoch_seconds < ?3 AND {USABLE}"),
        "DESC",
        "LIMIT 1",
        &[since],
    )?;
    let within = query(
        "sampled_at_epoch_seconds >= ?3 AND sampled_at_epoch_seconds <= ?4",
        "ASC",
        "",
        &[since, until],
    )?;
    let after = query(
        &format!("sampled_at_epoch_seconds > ?3 AND {USABLE}"),
        "ASC",
        "LIMIT 1",
        &[until],
    )?;
    before
        .iter()
        .chain(&within)
        .chain(&after)
        .map(|body| serde_json::from_str(body).context("decode stored quota probe"))
        .collect()
}

/// Save a fresh probe and, on success, its reset cache in one transaction.
/// Called on a blocking task through the only database writer.
pub fn save_quota_report(identity: &str, report: &ProfileQuota) -> Result<()> {
    let identity = identity.to_owned();
    let report = report.clone();
    submit_database_write("save_quota_report", move |connection| {
        save_report(connection, &identity, &report)
    })
}

fn save_report(connection: &mut Connection, identity: &str, report: &ProfileQuota) -> Result<()> {
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    // Preserve the observation before the reset cache fills missing fields.
    // Distinct probes can have identical values and timestamps.
    transaction.execute(
        "INSERT INTO quota_history(identity, profile_id, harness, sampled_at_epoch_seconds, body)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            identity,
            report.profile_id,
            report.harness.id(),
            report.refreshed_at_epoch_seconds,
            serde_json::to_string(report)?
        ],
    )?;
    if report.error.is_some() {
        transaction.commit()?;
        return Ok(());
    }
    let mut merged = report.clone();
    if !report.is_usage_priced()
        && let Some(previous) = load(&transaction, identity)?
    {
        mj_client::quota::merge_reset_windows(&mut merged.windows, &previous.windows);
    }
    transaction.execute("INSERT INTO quota_reset_cache(identity, body) VALUES (?1, ?2) ON CONFLICT(identity) DO UPDATE SET body = excluded.body",
        params![identity, serde_json::to_string(&merged)?])?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quota_history_retains_raw_probes_while_the_cache_preserves_reset_times() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota.sqlite3");
        let mut connection = open(&path).unwrap();
        let mut report = ProfileQuota {
            paid_credits: None,
            monthly_price_usd: Some(100),
            banked_resets: None,
            profile_id: "test".into(),
            harness: mj_core::config::HarnessKind::Claude,
            windows: vec![mj_client::quota::QuotaWindow {
                label: "5H".into(),
                remaining_percent: Some(0),
                duration_seconds: None,
                model_scope: None,
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: Some(12345),
            }],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 1,
            rate_limited_until_epoch_seconds: None,
            next_check_at_epoch_seconds: None,
        };
        let mut observed = vec![report.clone()];
        save_report(&mut connection, "account-a", &report).unwrap();
        for banked in [Some(2), Some(0), Some(1), None] {
            report.banked_resets = banked;
            report.paid_credits =
                banked.and_then(|balance| mj_client::quota::PaidCredits::limited(balance as f64));
            save_report(&mut connection, "account-a", &report).unwrap();
            observed.push(report.clone());
            assert_eq!(
                load(&connection, "account-a")
                    .unwrap()
                    .unwrap()
                    .banked_resets,
                banked
            );
            assert_eq!(
                load(&connection, "account-a")
                    .unwrap()
                    .unwrap()
                    .paid_credits,
                report.paid_credits
            );
        }
        // A fresh, identical reading is evidence of no change, even within
        // the same second. A partial reading must not inherit historical data.
        save_report(&mut connection, "account-a", &report).unwrap();
        observed.push(report.clone());
        report.refreshed_at_epoch_seconds += 600;
        report.monthly_price_usd = Some(200);
        report.windows[0].resets_at_epoch_seconds = None;
        report.windows.push(mj_client::quota::QuotaWindow {
            label: "Weekly (model)".into(),
            remaining_percent: None,
            duration_seconds: Some(7 * 24 * 3600),
            model_scope: Some("model".into()),
            used: Some(12),
            limit: Some(50),
            resets: None,
            resets_at_epoch_seconds: None,
        });
        save_report(&mut connection, "account-a", &report).unwrap();
        observed.push(report.clone());
        let last_good = load(&connection, "account-a").unwrap().unwrap();
        assert_eq!(last_good.monthly_price_usd, Some(200));
        assert_eq!(last_good.windows[0].resets_at_epoch_seconds, Some(12345));
        report.error = Some("rate limited".into());
        report.rate_limited_until_epoch_seconds = Some(1500);
        report.windows.clear();
        save_report(&mut connection, "account-a", &report).unwrap();
        observed.push(report.clone());
        assert_eq!(load(&connection, "account-a").unwrap(), Some(last_good));
        report.error = None;
        report.rate_limited_until_epoch_seconds = None;
        save_report(&mut connection, "account-a", &report).unwrap();
        observed.push(report.clone());
        // Different configurations under the same profile stay distinguishable.
        save_report(&mut connection, "account-b", &report).unwrap();
        drop(connection);
        let mut connection = open(&path).unwrap();
        assert_eq!(
            load(&connection, "account-a").unwrap().unwrap().windows[0].resets_at_epoch_seconds,
            Some(12345)
        );
        let history = connection
            .prepare("SELECT profile_id, harness, sampled_at_epoch_seconds, body FROM quota_history WHERE identity='account-a' ORDER BY id")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, u64>(2)?, row.get::<_, String>(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let decoded = history
            .into_iter()
            .map(|(profile, harness, at, body)| {
                let report: ProfileQuota = serde_json::from_str(&body).unwrap();
                assert_eq!(profile, report.profile_id);
                assert_eq!(harness, report.harness.id());
                assert_eq!(at, report.refreshed_at_epoch_seconds);
                report
            })
            .collect::<Vec<_>>();
        assert_eq!(decoded, observed);

        // The history insert and cache upsert must either both commit or neither.
        let cached = load(&connection, "account-a").unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER fail_quota_cache BEFORE INSERT ON quota_reset_cache
            BEGIN SELECT RAISE(ABORT, 'quota cache write failed'); END;",
            )
            .unwrap();
        report.refreshed_at_epoch_seconds += 600;
        assert!(save_report(&mut connection, "account-a", &report).is_err());
        assert_eq!(load(&connection, "account-a").unwrap(), cached);
        let count: usize = connection
            .query_row(
                "SELECT count(*) FROM quota_history WHERE identity='account-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, observed.len());
    }

    #[test]
    fn quota_history_bracketing_brackets_a_span_with_usable_probes_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut connection = open(&dir.path().join("quota.sqlite3")).unwrap();
        let probe = |at: u64, failed: bool| ProfileQuota {
            paid_credits: None,
            monthly_price_usd: None,
            banked_resets: None,
            profile_id: "test".into(),
            harness: mj_core::config::HarnessKind::Claude,
            windows: if failed {
                Vec::new()
            } else {
                vec![mj_client::quota::QuotaWindow {
                    label: "Weekly".into(),
                    remaining_percent: Some(50),
                    duration_seconds: None,
                    model_scope: None,
                    used: None,
                    limit: None,
                    resets: None,
                    resets_at_epoch_seconds: Some(1_000_000),
                }]
            },
            extra: None,
            error: failed.then(|| "provider down".into()),
            refreshed_at_epoch_seconds: at,
            rate_limited_until_epoch_seconds: None,
            next_check_at_epoch_seconds: None,
        };
        // Failed probes sit right outside both edges; another login's probe
        // sits closer to the span than any usable one of this login.
        for (identity, at, failed) in [
            ("a", 10, false),
            ("a", 50, true),
            ("a", 90, true),
            ("a", 100, false),
            ("a", 150, true),
            ("a", 200, false),
            ("a", 210, true),
            ("a", 250, false),
            ("a", 260, false),
            ("b", 95, false),
            ("b", 205, false),
        ] {
            save_report(&mut connection, identity, &probe(at, failed)).unwrap();
        }
        let times = |history: Vec<ProfileQuota>| {
            history
                .iter()
                .map(|probe| probe.refreshed_at_epoch_seconds)
                .collect::<Vec<_>>()
        };
        // Probes at the span's own edges are inside it; the failed ones inside
        // stay for the caller to skip.
        assert_eq!(
            times(history_bracketing(&connection, "a", "test", 100, 200).unwrap()),
            [10, 100, 150, 200, 250]
        );
        assert_eq!(
            times(history_bracketing(&connection, "a", "test", 101, 199).unwrap()),
            [100, 150, 200]
        );
        // An edge with no usable probe beyond it is simply absent.
        assert_eq!(
            times(history_bracketing(&connection, "a", "test", 5, 300).unwrap()),
            [10, 50, 90, 100, 150, 200, 210, 250, 260]
        );
        assert_eq!(
            times(history_bracketing(&connection, "a", "test", 300, 400).unwrap()),
            [260]
        );
    }
}

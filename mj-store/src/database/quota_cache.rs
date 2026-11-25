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

/// The probes that measure consumption over `since..until` (epoch seconds,
/// `until` exclusive), oldest first: the nearest probe before `since` that
/// `readable` accepts, every probe inside the span (readable or not), and the
/// nearest accepted probe at or after `until`. The caller decides what a
/// readable probe is (a failed probe or one without the reading it needs cannot
/// stand for the state at the span's edge), so no reading semantics live here.
/// Each edge is searched only within `edge_reach` seconds of the span, on
/// `quota_history_profile_time`, walking outward and stopping at the first
/// accepted probe; the caller states the reach its measurement is meaningful
/// over, so nothing older or newer is read or decoded. An edge with no accepted
/// probe in reach is absent, and the caller treats the span as not bracketed
/// there.
pub fn load_quota_history_bracketing(
    identity: &str,
    profile_id: &str,
    since: u64,
    until: u64,
    edge_reach: u64,
    readable: impl Fn(&ProfileQuota) -> bool,
) -> Result<Vec<ProfileQuota>> {
    history_bracketing(
        &*open_reader(&database_path())?,
        identity,
        profile_id,
        since,
        until,
        edge_reach,
        readable,
    )
}

fn history_bracketing(
    connection: &Connection,
    identity: &str,
    profile_id: &str,
    since: u64,
    until: u64,
    edge_reach: u64,
    readable: impl Fn(&ProfileQuota) -> bool,
) -> Result<Vec<ProfileQuota>> {
    let decode = |body: String| -> Result<ProfileQuota> {
        serde_json::from_str(&body).context("decode stored quota probe")
    };
    let edge = |range: &str, order: &str, bounds: [u64; 2]| -> Result<Option<ProfileQuota>> {
        let mut statement = connection.prepare(&format!(
            "SELECT body FROM quota_history
             WHERE profile_id = ?1 AND identity = ?2 AND {range}
             ORDER BY sampled_at_epoch_seconds {order}, id {order}"
        ))?;
        let mut rows = statement.query(params![profile_id, identity, bounds[0], bounds[1]])?;
        while let Some(row) = rows.next()? {
            let probe = decode(row.get(0)?)?;
            if readable(&probe) {
                return Ok(Some(probe));
            }
        }
        Ok(None)
    };
    let before = edge(
        "sampled_at_epoch_seconds < ?3 AND sampled_at_epoch_seconds >= ?4",
        "DESC",
        [since, since.saturating_sub(edge_reach)],
    )?;
    let within = connection
        .prepare(
            "SELECT body FROM quota_history
             WHERE profile_id = ?1 AND identity = ?2
               AND sampled_at_epoch_seconds >= ?3 AND sampled_at_epoch_seconds < ?4
             ORDER BY sampled_at_epoch_seconds ASC, id ASC",
        )?
        .query_map(params![profile_id, identity, since, until], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let after = edge(
        "sampled_at_epoch_seconds >= ?3 AND sampled_at_epoch_seconds <= ?4",
        "ASC",
        [until, until.saturating_add(edge_reach)],
    )?;
    before
        .into_iter()
        .map(Ok)
        .chain(within.into_iter().map(decode))
        .chain(after.into_iter().map(Ok))
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
    fn quota_history_bracketing_brackets_a_span_with_readable_probes_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut connection = open(&dir.path().join("quota.sqlite3")).unwrap();
        let window = |label: &str, scope: Option<&str>| mj_client::quota::QuotaWindow {
            label: label.into(),
            remaining_percent: Some(50),
            duration_seconds: None,
            model_scope: scope.map(str::to_owned),
            used: None,
            limit: None,
            resets: None,
            resets_at_epoch_seconds: Some(1_000_000),
        };
        // `w` weekly, `h` a 5-hour window only, `s` a model-scoped weekly only,
        // `f` a failed probe.
        let probe = |at: u64, kind: char| ProfileQuota {
            paid_credits: None,
            monthly_price_usd: None,
            banked_resets: None,
            profile_id: "test".into(),
            harness: mj_core::config::HarnessKind::Claude,
            windows: match kind {
                'w' => vec![window("5H", None), window("Weekly", None)],
                'h' => vec![window("5H", None)],
                's' => vec![window("Weekly", Some("fable"))],
                _ => Vec::new(),
            },
            extra: None,
            error: (kind == 'f').then(|| "provider down".into()),
            refreshed_at_epoch_seconds: at,
            rate_limited_until_epoch_seconds: None,
            next_check_at_epoch_seconds: None,
        };
        // Short-only, scoped-only and failed probes sit between each edge and
        // the nearest weekly reading; another login's probe sits closest.
        for (identity, at, kind) in [
            ("a", 10, 'w'),
            ("a", 50, 'h'),
            ("a", 90, 's'),
            ("a", 95, 'f'),
            ("a", 100, 'w'),
            ("a", 150, 'f'),
            ("a", 200, 'w'),
            ("a", 210, 'h'),
            ("a", 220, 's'),
            ("a", 250, 'w'),
            ("a", 260, 'w'),
            ("b", 99, 'w'),
            ("b", 205, 'w'),
        ] {
            save_report(&mut connection, identity, &probe(at, kind)).unwrap();
        }
        let weekly = |probe: &ProfileQuota| probe.windows.iter().any(|w| w.is_weekly_period());
        let times = |since, until, reach| {
            history_bracketing(&connection, "a", "test", since, until, reach, weekly)
                .unwrap()
                .iter()
                .map(|probe| probe.refreshed_at_epoch_seconds)
                .collect::<Vec<_>>()
        };
        // Probes at the span's start are inside it, those at its exclusive end
        // are the after edge, and in-range probes of every kind stay for the
        // caller to skip.
        assert_eq!(times(100, 201, 1000), [10, 100, 150, 200, 250]);
        assert_eq!(times(101, 200, 1000), [100, 150, 200]);
        // An edge with no readable probe beyond it is simply absent.
        assert_eq!(
            times(5, 301, 1000),
            [10, 50, 90, 95, 100, 150, 200, 210, 220, 250, 260]
        );
        assert_eq!(times(300, 400, 1000), [260]);
        assert_eq!(times(60, 80, 1000), [10, 100]);
        // A readable probe beyond the reach is not used, however near the
        // nearest readable one inside it is; within the reach it still is. The
        // reach bounds are inclusive.
        assert_eq!(times(100, 201, 40), [100, 150, 200]);
        assert_eq!(times(100, 201, 49), [100, 150, 200, 250]);
        assert_eq!(times(100, 201, 90), [10, 100, 150, 200, 250]);
        assert_eq!(times(60, 80, 50), [10, 100]);
        assert_eq!(times(60, 80, 49), [100]);
    }
}

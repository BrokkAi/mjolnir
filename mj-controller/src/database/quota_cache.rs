use super::*;
use crate::quota::ProfileQuota;

pub(crate) fn load_quota_cache(identity: &str) -> Result<Option<ProfileQuota>> {
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

/// Save a fresh probe and, on success, its reset cache in one transaction.
/// Called on a blocking task through the only database writer.
pub(crate) fn save_quota_report(identity: &str, report: &ProfileQuota) -> Result<()> {
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
        crate::quota::merge_reset_windows(&mut merged.windows, &previous.windows);
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
            banked_resets: None,
            profile_id: "test".into(),
            harness: mj_core::config::HarnessKind::Claude,
            windows: vec![crate::quota::QuotaWindow {
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
        };
        let mut observed = vec![report.clone()];
        save_report(&mut connection, "account-a", &report).unwrap();
        for banked in [Some(2), Some(0), Some(1), None] {
            report.banked_resets = banked;
            save_report(&mut connection, "account-a", &report).unwrap();
            observed.push(report.clone());
            assert_eq!(
                load(&connection, "account-a")
                    .unwrap()
                    .unwrap()
                    .banked_resets,
                banked
            );
        }
        // A fresh, identical reading is evidence of no change, even within
        // the same second. A partial reading must not inherit historical data.
        save_report(&mut connection, "account-a", &report).unwrap();
        observed.push(report.clone());
        report.refreshed_at_epoch_seconds += 600;
        report.windows[0].resets_at_epoch_seconds = None;
        report.windows.push(crate::quota::QuotaWindow {
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
}

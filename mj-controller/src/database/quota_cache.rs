use super::*;
use crate::quota::ProfileQuota;

pub(crate) fn load_quota_cache(identity: &str) -> Result<Option<ProfileQuota>> {
    load(&open(&database_path())?, identity)
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

pub(crate) fn save_quota_cache(identity: &str, report: &ProfileQuota) -> Result<()> {
    save(&mut open(&database_path())?, identity, report)
}

fn save(connection: &mut Connection, identity: &str, report: &ProfileQuota) -> Result<()> {
    if report.error.is_some() {
        return Ok(());
    }
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
    fn failed_and_partial_reports_preserve_reset_times_across_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quota.sqlite3");
        let mut connection = open(&path).unwrap();
        let mut report = ProfileQuota {
            profile_id: "test".into(),
            harness: mj_core::config::HarnessKind::Claude,
            windows: vec![crate::quota::QuotaWindow {
                label: "5H".into(),
                remaining_percent: Some(0),
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: Some(12345),
            }],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 1,
        };
        save(&mut connection, "account-a", &report).unwrap();
        report.error = Some("rate limited".into());
        report.windows.clear();
        save(&mut connection, "account-a", &report).unwrap();
        report.error = None;
        save(&mut connection, "account-a", &report).unwrap();
        drop(connection);
        let connection = open(&path).unwrap();
        assert_eq!(
            load(&connection, "account-a").unwrap().unwrap().windows[0].resets_at_epoch_seconds,
            Some(12345)
        );
        assert!(load(&connection, "account-b").unwrap().is_none());
    }
}

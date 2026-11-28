use super::*;
use mj_core::config::{RoleAssignmentSnapshot, diff_role_assignments};
use mj_core::usage::RoleAssignmentChange;

pub fn sync_role_assignment_snapshot(
    snapshot: &RoleAssignmentSnapshot,
    now_ms: i64,
) -> Result<Option<RoleAssignmentChange>> {
    sync_role_assignment_snapshot_at(&database_path(), snapshot, now_ms)
}

pub fn sync_role_assignment_snapshot_at(
    path: &Path,
    snapshot: &RoleAssignmentSnapshot,
    now_ms: i64,
) -> Result<Option<RoleAssignmentChange>> {
    let mut connection = schema::open_writer(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let previous: Option<String> = tx
        .query_row(
            "SELECT snapshot_json FROM role_assignment_state WHERE id=1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let old = previous
        .map(|json| serde_json::from_str::<RoleAssignmentSnapshot>(&json))
        .transpose()?;
    let summary = old
        .as_ref()
        .and_then(|old| diff_role_assignments(old, snapshot));
    if old.as_ref() != Some(snapshot) {
        tx.execute("INSERT INTO role_assignment_state(id, snapshot_json, updated_at_ms) VALUES (1, ?1, ?2) ON CONFLICT(id) DO UPDATE SET snapshot_json=excluded.snapshot_json, updated_at_ms=excluded.updated_at_ms", params![serde_json::to_string(snapshot)?, now_ms])?;
    }
    let change = if let Some(summary) = summary {
        tx.execute(
            "INSERT INTO role_assignment_changes(changed_at_ms, summary) VALUES (?1, ?2)",
            params![now_ms, summary],
        )?;
        Some(RoleAssignmentChange {
            id: tx.last_insert_rowid(),
            changed_at_ms: now_ms,
            summary,
        })
    } else {
        None
    };
    tx.commit()?;
    Ok(change)
}

pub fn load_role_assignment_changes(
    since_ms: i64,
    until_ms: i64,
) -> Result<Vec<RoleAssignmentChange>> {
    load_role_assignment_changes_at(&database_path(), since_ms, until_ms)
}

pub fn load_role_assignment_changes_at(
    path: &Path,
    since_ms: i64,
    until_ms: i64,
) -> Result<Vec<RoleAssignmentChange>> {
    let mut connection = open_reader(path)?;
    let tx = connection.transaction()?;
    load_role_assignment_changes_tx(&tx, since_ms, until_ms)
}

pub(super) fn load_role_assignment_changes_tx(
    tx: &Transaction<'_>,
    since_ms: i64,
    until_ms: i64,
) -> Result<Vec<RoleAssignmentChange>> {
    let mut stmt = tx.prepare("SELECT id, changed_at_ms, summary FROM role_assignment_changes WHERE changed_at_ms BETWEEN ?1 AND ?2 ORDER BY changed_at_ms, id")?;
    Ok(stmt
        .query_map(params![since_ms, until_ms], |row| {
            Ok(RoleAssignmentChange {
                id: row.get(0)?,
                changed_at_ms: row.get(1)?,
                summary: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::config::{Config, SubagentChoice};

    #[test]
    fn role_assignment_sync_is_persistent_and_idempotent() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("role.sqlite");
        let mut snapshot = RoleAssignmentSnapshot::from_config(&Config::default());
        assert!(sync_role_assignment_snapshot_at(&path, &snapshot, 1)?.is_none());
        snapshot.primary.push(SubagentChoice {
            model: "example".into(),
            effort: None,
        });
        let change = sync_role_assignment_snapshot_at(&path, &snapshot, 2)?.unwrap();
        assert_eq!(change.summary, "Primary: off → example");
        assert!(sync_role_assignment_snapshot_at(&path, &snapshot, 3)?.is_none());
        let mut connection = open_reader(&path)?;
        let tx = connection.transaction()?;
        assert_eq!(load_role_assignment_changes_tx(&tx, 0, 3)?, vec![change]);
        Ok(())
    }
}

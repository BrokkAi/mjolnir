//! Transactional SQLite persistence for shared EC2 Move ingress grants and
//! the daemon's stable instance identifier.

use std::collections::{BTreeMap, BTreeSet};

use super::*;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Ec2MoveGrantKeyRow {
    pub security_group_id: String,
    pub cidr_ip: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ec2MoveGrantRow {
    pub key: Ec2MoveGrantKeyRow,
    pub state: String,
    pub users: BTreeSet<String>,
    pub absent_checks: u32,
    pub attempt_id: u64,
}

/// Return the stable daemon instance ID recorded in the store, generating and
/// persisting one on first use.
pub fn daemon_instance_id() -> Result<String> {
    submit_database_write("daemon_instance_id", |connection| {
        let tx = connection.transaction()?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT instance_id FROM daemon_instance_metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            tx.commit()?;
            return Ok(id);
        }
        let id = mj_core::workspace::new_workspace_id()?;
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO daemon_instance_metadata(singleton, instance_id, created_at)
             VALUES (1, ?1, ?2)",
            params![id, now],
        )?;
        tx.commit()?;
        Ok(id)
    })
}

/// Run an atomic read-modify-write transaction over all `ec2_move_grants` rows.
pub fn with_ec2_move_grants_transaction<R, F>(f: F) -> Result<R>
where
    R: Send + 'static,
    F: FnOnce(&mut BTreeMap<Ec2MoveGrantKeyRow, Ec2MoveGrantRow>) -> Result<R> + Send + 'static,
{
    submit_database_write("ec2_move_grants_tx", move |connection| {
        let tx = connection.transaction()?;
        let mut records = BTreeMap::new();
        {
            let mut stmt = tx.prepare(
                "SELECT security_group_id, cidr_ip, port, state, users_json, absent_checks, attempt_id
                 FROM ec2_move_grants",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })?;
            for row in rows {
                let (sg, cidr, port, state, users_json, absent_checks, attempt_id) = row?;
                let key = Ec2MoveGrantKeyRow {
                    security_group_id: sg,
                    cidr_ip: cidr,
                    port: u16::try_from(port).context("invalid ec2_move_grants port")?,
                };
                let users: BTreeSet<String> = serde_json::from_str(&users_json)
                    .context("decode ec2_move_grants.users_json")?;
                records.insert(
                    key.clone(),
                    Ec2MoveGrantRow {
                        key,
                        state,
                        users,
                        absent_checks: u32::try_from(absent_checks)
                            .context("invalid ec2_move_grants absent_checks")?,
                        attempt_id: u64::try_from(attempt_id)
                            .context("invalid ec2_move_grants attempt_id")?,
                    },
                );
            }
        }
        let before_keys: BTreeSet<Ec2MoveGrantKeyRow> = records.keys().cloned().collect();
        let result = f(&mut records)?;
        let now = Utc::now().to_rfc3339();
        for key in &before_keys {
            if !records.contains_key(key) {
                tx.execute(
                    "DELETE FROM ec2_move_grants
                     WHERE security_group_id = ?1 AND cidr_ip = ?2 AND port = ?3",
                    params![key.security_group_id, key.cidr_ip, i64::from(key.port)],
                )?;
            }
        }
        for (key, row) in &records {
            let users_json = serde_json::to_string(&row.users)?;
            tx.execute(
                "INSERT INTO ec2_move_grants(
                     security_group_id, cidr_ip, port, state, users_json, absent_checks, attempt_id, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(security_group_id, cidr_ip, port) DO UPDATE SET
                     state = excluded.state,
                     users_json = excluded.users_json,
                     absent_checks = excluded.absent_checks,
                     attempt_id = excluded.attempt_id,
                     updated_at = excluded.updated_at",
                params![
                    key.security_group_id,
                    key.cidr_ip,
                    i64::from(key.port),
                    row.state,
                    users_json,
                    i64::from(row.absent_checks),
                    i64::try_from(row.attempt_id).context("ec2_move_grants attempt_id overflow")?,
                    now,
                ],
            )?;
        }
        tx.commit()?;
        Ok(result)
    })
}

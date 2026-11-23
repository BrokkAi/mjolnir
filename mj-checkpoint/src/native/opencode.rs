//! The source key and private database layout used by OpenCode imports.

use std::path::Path;

use anyhow::{Result, ensure};
use rusqlite::Connection;

pub(crate) const DATABASE: &str = ".data/opencode/opencode.db";

/// Split SessionWiki's shared SQLite key into its physical file and literal ID.
pub fn database_key(path: &Path) -> Option<(&Path, &str)> {
    let (database, id) = path.to_str()?.split_once('\u{1f}')?;
    (!database.is_empty() && !id.is_empty()).then_some((Path::new(database), id))
}

/// The pinned ACP loader only sees v2 rows. Its v1 migration runs in the
/// background and can finish after the first session/load.
pub(crate) fn has_resumable_session(conn: &Connection, id: &str) -> Result<bool> {
    if !table_exists(conn, "session_v2")? {
        return Ok(false);
    }
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM session_v2 WHERE id = ?1)",
        [id],
        |row| row.get(0),
    )?)
}

fn table_exists(conn: &Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE type='table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )
}

/// Strip other sessions and profile data from the private copy of a shared
/// store. Preserve the schema and migration state so the pinned CLI can open
/// it normally and recognizes which schema changes have already run.
pub(crate) fn retain_session(copy: &Connection, id: &str) -> Result<()> {
    copy.pragma_update(None, "foreign_keys", false)?;
    copy.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_ENABLE_TRIGGER,
        false,
    )?;
    let tx = copy.unchecked_transaction()?;
    let tables = tx
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    ensure!(
        has_resumable_session(&tx, id)?,
        "OpenCode session {id} is absent from the v2 store. Open OpenCode and let it finish migrating its session database, then retry."
    );
    for table in ["session", "session_v2"] {
        if tables.iter().any(|name| name == table) {
            tx.execute(&format!("DELETE FROM {table} WHERE id IS NOT ?1"), [id])?;
        }
    }
    let referenced_ids = |column: &str| -> Result<Vec<String>> {
        let mut ids = Vec::new();
        for table in ["session", "session_v2"] {
            if !tables.iter().any(|name| name == table) {
                continue;
            }
            let exists: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
                [table, column],
                |row| row.get(0),
            )?;
            if exists {
                ids.extend(
                    tx.prepare(&format!(
                        "SELECT {column} FROM {table} WHERE {column} IS NOT NULL"
                    ))?
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                );
            }
        }
        Ok(ids)
    };
    let projects = serde_json::to_string(&referenced_ids("project_id")?)?;
    let workspaces = serde_json::to_string(&referenced_ids("workspace_id")?)?;
    // Data is allowlisted per table. In particular permissions, logins,
    // share secrets and arbitrary key-value records are profile data.
    let tables_have_blob_refs = tables.iter().any(|table| table == "blob_ref");
    let tables_have_instruction_state = tables.iter().any(|table| table == "instruction_state");
    for table in tables {
        let name = table.replace('"', "\"\"");
        let delete = format!("DELETE FROM \"{name}\"");
        match table.as_str() {
            "session" | "session_v2" | "migration" | "__drizzle_migrations" => {}
            "message"
            | "part"
            | "todo"
            | "session_message"
            | "session_pending"
            | "session_part"
            | "session_inbox"
            | "blob_ref"
            | "session_context_epoch"
            | "session_input"
            | "instruction_entry"
            | "instruction_state" => {
                tx.execute(&format!("{delete} WHERE session_id IS NOT ?1"), [id])?;
            }
            "event" | "event_sequence" => {
                tx.execute(&format!("{delete} WHERE aggregate_id IS NOT ?1"), [id])?;
            }
            "blob" if tables_have_blob_refs => {
                tx.execute(
                    &format!(
                        "{delete} WHERE hash NOT IN (SELECT hash FROM blob_ref WHERE session_id = ?1 AND hash IS NOT NULL)"
                    ),
                    [id],
                )?;
            }
            "instruction_blob" if tables_have_instruction_state => {
                tx.execute(
                    &format!(
                        "{delete} WHERE hash NOT IN (
                    SELECT value FROM instruction_state, json_each(initial_values) WHERE session_id = ?1 AND value IS NOT NULL
                    UNION
                    SELECT value FROM instruction_state, json_each(current_values) WHERE session_id = ?1 AND value IS NOT NULL
                )"
                    ),
                    [id],
                )?;
            }
            "project" => {
                tx.execute(
                    &format!(
                        "{delete} WHERE id NOT IN (SELECT value FROM json_each(?1) WHERE value IS NOT NULL)"
                    ),
                    [&projects],
                )?;
            }
            "workspace" => {
                tx.execute(
                    &format!(
                        "{delete} WHERE id NOT IN (SELECT value FROM json_each(?1) WHERE value IS NOT NULL)"
                    ),
                    [&workspaces],
                )?;
            }
            "kv" => {
                tx.execute(&format!("{delete} WHERE key IS NOT 'migration.v1-v2'"), [])?;
            }
            _ => {
                tx.execute(&delete, [])?;
            }
        }
    }
    tx.commit()?;
    // Free pages in a SQLite backup can retain removed records.
    copy.execute_batch("VACUUM")?;
    Ok(())
}

/// OpenCode's ACP loader requires the saved directory to match the cwd in
/// session/load. The session ID selects the row even in older shared copies.
pub(crate) fn relocate(data: &[u8], id: &str, target_cwd: &Path) -> Result<Vec<u8>> {
    use anyhow::Context;

    let cwd = target_cwd
        .to_str()
        .context("OpenCode working directory is not UTF-8")?;
    let scratch = tempfile::tempdir()?;
    let path = scratch.path().join("opencode.db");
    std::fs::write(&path, data)?;
    {
        let mut copy = Connection::open(&path)?;
        let tx = copy.transaction()?;
        for table in ["session", "session_v2"] {
            if table_exists(&tx, table)? {
                tx.execute(
                    &format!(
                        "UPDATE {table} SET directory = ?1 WHERE id = ?2 AND directory IS NOT ?1"
                    ),
                    [cwd, id],
                )?;
            }
        }
        tx.commit()?;
        copy.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    }
    Ok(std::fs::read(&path)?)
}

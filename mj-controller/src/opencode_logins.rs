//! OpenCode 2's logins, read from its own database.
//!
//! OpenCode 2 keeps every login in the `credential` table of `opencode.db` in
//! its data directory, the same file that holds every session it ran there.
//! Staging carries only the logins into a session home, as the JSON array
//! `opencode auth export` prints and `opencode auth import` reads; each launch
//! imports it ([`mj_core::harness_runtime::opencode_acp_script`]).
//!
//! The database is read here rather than through `opencode auth export`
//! because the daemon cannot rely on finding the person's own `opencode`, and
//! Mjolnir's pinned one would migrate a database the person's OpenCode owns.
//! A read-only query changes nothing, and a schema it does not expect fails
//! the read with SQLite's error rather than staging a session without logins.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use mj_core::config::{HarnessKind, HarnessProfile};
use rusqlite::{Connection, OpenFlags};

/// The database file name of every released OpenCode channel.
const DATABASE: &str = "opencode.db";

/// The logins OpenCode keeps for `profile`, in `auth export` form. `None` when
/// its data directory has no OpenCode database or the database has no logins.
pub fn profile_logins(profile: &HarnessProfile) -> Result<Option<Vec<u8>>> {
    let data = HarnessKind::OpenCode
        .nested_data_dir(&profile.home)
        .expect("OpenCode has an XDG home");
    logins(&data)
}

/// The logins in the OpenCode data directory `data`, in `auth export` form:
/// one entry per stored login, with the fields `auth import` reads. `None`
/// when `data` has no OpenCode database or the database has no logins.
pub fn logins(data: &Path) -> Result<Option<Vec<u8>>> {
    let path = data.join(DATABASE);
    if !path.is_file() {
        return Ok(None);
    }
    let read = || -> Result<Vec<serde_json::Value>> {
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // The person's own OpenCode may be writing; a reader waits out its
        // brief write locks instead of failing the launch.
        connection.busy_timeout(Duration::from_secs(5))?;
        let mut statement = connection.prepare(
            "SELECT id, integration_id, label, active, value FROM credential \
             WHERE integration_id IS NOT NULL ORDER BY time_created, id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<bool>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (id, integration, label, active, value) = row?;
            let value: serde_json::Value = serde_json::from_str(&value)
                .with_context(|| format!("decode stored login {id}"))?;
            Ok(serde_json::json!({
                "id": id,
                "integrationID": integration,
                "label": label,
                "active": active.unwrap_or(false),
                "value": value,
            }))
        })
        .collect()
    };
    let entries =
        read().with_context(|| format!("read OpenCode logins from {}", path.display()))?;
    if entries.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::to_vec_pretty(&entries)?))
}

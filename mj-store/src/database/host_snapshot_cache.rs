use super::*;

/// A host-side snapshot archive copy tracked independently of `session_checkpoints`.
///
/// Each row is inserted before the archive file is written and deleted only
/// after the file is confirmed removed (or absent), so copies superseded by a
/// later suspend, a purge, or a Destroy remain tracked for cleanup and 48-hour
/// expiry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSnapshotCacheEntry {
    pub id: i64,
    pub session_id: String,
    pub host: String,
    pub sha256: String,
    pub path: String,
    pub written_at: String,
}

fn entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HostSnapshotCacheEntry> {
    Ok(HostSnapshotCacheEntry {
        id: row.get(0)?,
        session_id: row.get(1)?,
        host: row.get(2)?,
        sha256: row.get(3)?,
        path: row.get(4)?,
        written_at: row.get(5)?,
    })
}

/// Record a host snapshot cache entry BEFORE writing the archive file to disk.
pub fn record_host_snapshot_cache_entry(
    session_id: &str,
    host: &str,
    sha256: &str,
    path: &str,
    written_at: &str,
) -> Result<i64> {
    let connection = open(&database_path())?;
    connection
        .execute(
            "INSERT INTO host_snapshot_cache (session_id, host, sha256, path, written_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, host, sha256, path, written_at],
        )
        .context("insert host_snapshot_cache row")?;
    Ok(connection.last_insert_rowid())
}

/// Look up cached snapshot entries for `(host, sha256)`, newest first.
pub fn lookup_host_snapshot_cache_entries(
    host: &str,
    sha256: &str,
) -> Result<Vec<HostSnapshotCacheEntry>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection
        .prepare(
            "SELECT id, session_id, host, sha256, path, written_at
             FROM host_snapshot_cache
             WHERE host = ?1 AND sha256 = ?2
             ORDER BY written_at DESC, id DESC",
        )
        .context("prepare host_snapshot_cache lookup")?;
    let rows = statement
        .query_map(params![host, sha256], entry_from_row)
        .context("query host_snapshot_cache by host and sha256")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("collect host_snapshot_cache lookup rows")
}

/// List all cached snapshot entries belonging to `session_id` (including copies
/// superseded by later suspends).
pub fn list_host_snapshot_cache_entries_for_session(
    session_id: &str,
) -> Result<Vec<HostSnapshotCacheEntry>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection
        .prepare(
            "SELECT id, session_id, host, sha256, path, written_at
             FROM host_snapshot_cache
             WHERE session_id = ?1
             ORDER BY written_at ASC, id ASC",
        )
        .context("prepare host_snapshot_cache session list")?;
    let rows = statement
        .query_map(params![session_id], entry_from_row)
        .context("query host_snapshot_cache by session_id")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("collect host_snapshot_cache session rows")
}

/// List all cached snapshot entries whose `written_at <= cutoff_rfc3339`.
pub fn list_overdue_host_snapshot_cache_entries(
    cutoff_rfc3339: &str,
) -> Result<Vec<HostSnapshotCacheEntry>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection
        .prepare(
            "SELECT id, session_id, host, sha256, path, written_at
             FROM host_snapshot_cache
             WHERE written_at <= ?1
             ORDER BY written_at ASC, id ASC",
        )
        .context("prepare host_snapshot_cache overdue list")?;
    let rows = statement
        .query_map(params![cutoff_rfc3339], entry_from_row)
        .context("query overdue host_snapshot_cache entries")?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("collect overdue host_snapshot_cache rows")
}

/// Return the earliest `written_at` timestamp across all `host_snapshot_cache`
/// rows, used by the daemon's expiry timer to sleep until the next due removal.
pub fn earliest_host_snapshot_cache_written_at() -> Result<Option<String>> {
    let connection = open_reader(&database_path())?;
    connection
        .query_row("SELECT MIN(written_at) FROM host_snapshot_cache", [], |row| {
            row.get::<_, Option<String>>(0)
        })
        .context("query earliest host_snapshot_cache written_at")
}

/// Delete a `host_snapshot_cache` row by ID. Callers must invoke this only
/// after the underlying file is confirmed removed or already absent.
pub fn delete_host_snapshot_cache_entry(id: i64) -> Result<bool> {
    let connection = open(&database_path())?;
    let deleted = connection
        .execute("DELETE FROM host_snapshot_cache WHERE id = ?1", params![id])
        .context("delete host_snapshot_cache row")?;
    Ok(deleted > 0)
}

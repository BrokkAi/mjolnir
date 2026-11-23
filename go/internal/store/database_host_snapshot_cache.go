// Port of mj-store/src/database/host_snapshot_cache.rs.

package store

import (
	"database/sql"
	"fmt"
)

// HostSnapshotCacheEntry is a host-side snapshot archive copy tracked
// independently of session_checkpoints.
//
// Each row is inserted before the archive file is written and deleted only
// after the file is confirmed removed (or absent), so copies superseded by a
// later suspend, a purge, or a Destroy remain tracked for cleanup and 48-hour
// expiry.
type HostSnapshotCacheEntry struct {
	ID        int64
	SessionID string
	Host      string
	SHA256    string
	Path      string
	WrittenAt string
}

func entryFromRow(rows *sql.Rows) (HostSnapshotCacheEntry, error) {
	var entry HostSnapshotCacheEntry
	err := rows.Scan(&entry.ID, &entry.SessionID, &entry.Host, &entry.SHA256, &entry.Path, &entry.WrittenAt)
	return entry, err
}

// RecordHostSnapshotCacheEntry records a host snapshot cache entry BEFORE
// writing the archive file to disk.
func RecordHostSnapshotCacheEntry(sessionID, host, sha256, path, writtenAt string) (int64, error) {
	connection, err := open(DatabasePath())
	if err != nil {
		return 0, err
	}
	defer connection.Close()
	result, err := connection.Exec(`INSERT INTO host_snapshot_cache (session_id, host, sha256, path, written_at)
             VALUES (?1, ?2, ?3, ?4, ?5)`, sessionID, host, sha256, path, writtenAt)
	if err != nil {
		return 0, fmt.Errorf("insert host_snapshot_cache row: %w", err)
	}
	return result.LastInsertId()
}

// hostSnapshotCacheList is one of the list queries over the cache's rows,
// with the context of each of its steps.
func hostSnapshotCacheList(query string, args []any, prepare, run, collect string) ([]HostSnapshotCacheEntry, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	statement, err := connection.Prepare(query)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", prepare, err)
	}
	defer statement.Close()
	rows, err := statement.Query(args...)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", run, err)
	}
	defer rows.Close()
	entries := []HostSnapshotCacheEntry{}
	for rows.Next() {
		entry, err := entryFromRow(rows)
		if err != nil {
			return nil, fmt.Errorf("%s: %w", collect, err)
		}
		entries = append(entries, entry)
	}
	if err := rows.Err(); err != nil {
		return nil, fmt.Errorf("%s: %w", collect, err)
	}
	return entries, nil
}

// LookupHostSnapshotCacheEntries looks up cached snapshot entries for (host,
// sha256), newest first.
func LookupHostSnapshotCacheEntries(host, sha256 string) ([]HostSnapshotCacheEntry, error) {
	return hostSnapshotCacheList(`SELECT id, session_id, host, sha256, path, written_at
             FROM host_snapshot_cache
             WHERE host = ?1 AND sha256 = ?2
             ORDER BY written_at DESC, id DESC`, []any{host, sha256},
		"prepare host_snapshot_cache lookup",
		"query host_snapshot_cache by host and sha256",
		"collect host_snapshot_cache lookup rows")
}

// ListHostSnapshotCacheEntriesForSession lists all cached snapshot entries
// belonging to sessionID (including copies superseded by later suspends).
func ListHostSnapshotCacheEntriesForSession(sessionID string) ([]HostSnapshotCacheEntry, error) {
	return hostSnapshotCacheList(`SELECT id, session_id, host, sha256, path, written_at
             FROM host_snapshot_cache
             WHERE session_id = ?1
             ORDER BY written_at ASC, id ASC`, []any{sessionID},
		"prepare host_snapshot_cache session list",
		"query host_snapshot_cache by session_id",
		"collect host_snapshot_cache session rows")
}

// ListOverdueHostSnapshotCacheEntries lists all cached snapshot entries
// whose written_at <= cutoffRFC3339.
func ListOverdueHostSnapshotCacheEntries(cutoffRFC3339 string) ([]HostSnapshotCacheEntry, error) {
	return hostSnapshotCacheList(`SELECT id, session_id, host, sha256, path, written_at
             FROM host_snapshot_cache
             WHERE written_at <= ?1
             ORDER BY written_at ASC, id ASC`, []any{cutoffRFC3339},
		"prepare host_snapshot_cache overdue list",
		"query overdue host_snapshot_cache entries",
		"collect overdue host_snapshot_cache rows")
}

// EarliestHostSnapshotCacheWrittenAt returns the earliest written_at
// timestamp across all host_snapshot_cache rows, used by the daemon's expiry
// timer to sleep until the next due removal.
func EarliestHostSnapshotCacheWrittenAt() (*string, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	var earliest *string
	if err := connection.QueryRow("SELECT MIN(written_at) FROM host_snapshot_cache").Scan(&earliest); err != nil {
		return nil, fmt.Errorf("query earliest host_snapshot_cache written_at: %w", err)
	}
	return earliest, nil
}

// DeleteHostSnapshotCacheEntry deletes a host_snapshot_cache row by ID.
// Callers must invoke this only after the underlying file is confirmed
// removed or already absent.
func DeleteHostSnapshotCacheEntry(id int64) (bool, error) {
	connection, err := open(DatabasePath())
	if err != nil {
		return false, err
	}
	defer connection.Close()
	result, err := connection.Exec("DELETE FROM host_snapshot_cache WHERE id = ?1", id)
	if err != nil {
		return false, fmt.Errorf("delete host_snapshot_cache row: %w", err)
	}
	deleted, err := result.RowsAffected()
	if err != nil {
		return false, fmt.Errorf("delete host_snapshot_cache row: %w", err)
	}
	return deleted > 0, nil
}

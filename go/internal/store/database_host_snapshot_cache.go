// Port of mj-store/src/database/host_snapshot_cache.rs.

package store

import (
	"database/sql"
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
	panic("unported: mj-store/src/database/host_snapshot_cache.rs record_host_snapshot_cache_entry")
}

// LookupHostSnapshotCacheEntries looks up cached snapshot entries for (host,
// sha256), newest first.
func LookupHostSnapshotCacheEntries(host, sha256 string) ([]HostSnapshotCacheEntry, error) {
	panic("unported: mj-store/src/database/host_snapshot_cache.rs lookup_host_snapshot_cache_entries")
}

// ListHostSnapshotCacheEntriesForSession lists all cached snapshot entries
// belonging to sessionID (including copies superseded by later suspends).
func ListHostSnapshotCacheEntriesForSession(sessionID string) ([]HostSnapshotCacheEntry, error) {
	panic("unported: mj-store/src/database/host_snapshot_cache.rs list_host_snapshot_cache_entries_for_session")
}

// ListOverdueHostSnapshotCacheEntries lists all cached snapshot entries
// whose written_at <= cutoffRFC3339.
func ListOverdueHostSnapshotCacheEntries(cutoffRFC3339 string) ([]HostSnapshotCacheEntry, error) {
	panic("unported: mj-store/src/database/host_snapshot_cache.rs list_overdue_host_snapshot_cache_entries")
}

// EarliestHostSnapshotCacheWrittenAt returns the earliest written_at
// timestamp across all host_snapshot_cache rows, used by the daemon's expiry
// timer to sleep until the next due removal.
func EarliestHostSnapshotCacheWrittenAt() (*string, error) {
	panic("unported: mj-store/src/database/host_snapshot_cache.rs earliest_host_snapshot_cache_written_at")
}

// DeleteHostSnapshotCacheEntry deletes a host_snapshot_cache row by ID.
// Callers must invoke this only after the underlying file is confirmed
// removed or already absent.
func DeleteHostSnapshotCacheEntry(id int64) (bool, error) {
	panic("unported: mj-store/src/database/host_snapshot_cache.rs delete_host_snapshot_cache_entry")
}

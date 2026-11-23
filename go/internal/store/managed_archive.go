// Port of mj-store/src/managed_archive.rs.
//
// The file names of the checkpoint archives Mjolnir manages.

package store

// ManagedCheckpointArchiveName is the session and checkpoint generation a
// managed archive file name names.
//
// Managed checkpoints are named `<session_id>-<frontier>-archive-<32 hex>`,
// so the file name alone says which session a checkpoint belongs to and which
// of that session's checkpoints is the newest.
type ManagedCheckpointArchiveName struct {
	SessionID string
	Frontier  uint64
}

// ParseManagedCheckpointArchiveName is Rust's managed_checkpoint_archive_name
// (renamed because Go cannot give the function and the
// ManagedCheckpointArchiveName type the same name). It parses a managed
// checkpoint archive file name, or returns nil when the name is not one
// Mjolnir wrote. name is the file name (Rust's OsStr); a name that is not
// valid UTF-8 is not one Mjolnir wrote.
func ParseManagedCheckpointArchiveName(name string) *ManagedCheckpointArchiveName {
	panic("unported: mj-store/src/managed_archive.rs managed_checkpoint_archive_name")
}

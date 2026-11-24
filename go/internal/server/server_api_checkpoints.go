// Port of mj-server/src/server/api/checkpoints.rs (only the wire types so far).
//
// `POST /api/v1/checkpoints/purge-untracked`. The daemon rewrites each
// selected archive under that session's lifecycle admission.

package server

// PurgeUntrackedRequest removes legacy untracked payloads from one stopped
// session, or from every session the daemon holds. Rust:
// deny_unknown_fields.
type PurgeUntrackedRequest struct {
	SessionID *string `json:"session_id" serde:"default"`
	All       bool    `json:"all" serde:"default"`
	DryRun    bool    `json:"dry_run" serde:"default"`
}

// PurgeUntrackedStatus is how one session's archive was treated
// (snake_case).
type PurgeUntrackedStatus string

const (
	PurgeUntrackedStatusPurged  PurgeUntrackedStatus = "purged"
	PurgeUntrackedStatusDryRun  PurgeUntrackedStatus = "dry_run"
	PurgeUntrackedStatusSkipped PurgeUntrackedStatus = "skipped"
	// PurgeUntrackedStatusFailed: this session's archive could not be read.
	// A batch keeps going.
	PurgeUntrackedStatusFailed PurgeUntrackedStatus = "failed"
)

// PurgeUntrackedStashNote is a stash commit that still names untracked
// files. Purge leaves the bundle alone and reports the commit so those files
// are not a surprise on resume. Rust: deny_unknown_fields.
type PurgeUntrackedStashNote struct {
	RepositoryID string `json:"repository_id"`
	Commit       string `json:"commit"`
	Message      string `json:"message"`
}

// PurgeUntrackedSession is one session in a purge report. Rust:
// deny_unknown_fields.
type PurgeUntrackedSession struct {
	SessionID           string                    `json:"session_id"`
	Status              PurgeUntrackedStatus      `json:"status"`
	BytesReclaimed      uint64                    `json:"bytes_reclaimed"`
	Reason              *string                   `json:"reason" serde:"default"`
	StashUntrackedNotes []PurgeUntrackedStashNote `json:"stash_untracked_notes" serde:"default"`
	Warnings            []string                  `json:"warnings" serde:"default"`
}

// PurgeUntrackedResponse is the purge the daemon finished, including a dry
// run. Rust: deny_unknown_fields.
type PurgeUntrackedResponse struct {
	Sessions            []PurgeUntrackedSession `json:"sessions"`
	TotalBytesReclaimed uint64                  `json:"total_bytes_reclaimed"`
	DryRun              bool                    `json:"dry_run"`
}

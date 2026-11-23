// Port of mj-store/src/database.rs.

// Normalized controller state and composer history stored in SQLite.
//
// Rust's database module root also re-exports its submodules; in Go they are
// the database_*.go files of this package.

package store

import (
	"bytes"
	"context"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"sort"
	"strings"

	"modernc.org/sqlite"
	sqlite3 "modernc.org/sqlite/lib"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const schemaVersion int64 = 153

// Connection is rusqlite's Connection as the store's functions take it: one
// SQLite connection, or a transaction open on one (a rusqlite Transaction
// dereferences to its Connection, so a Rust function taking &Connection
// accepts both). *sql.Conn and *sql.Tx satisfy it.
//
// A function that opens its own transaction takes *sql.Conn instead (Rust's
// &mut Connection, or an unchecked_transaction on &Connection, which cannot
// nest), and one that must run inside its caller's transaction takes *sql.Tx
// (Rust's &Transaction).
type Connection interface {
	ExecContext(ctx context.Context, query string, args ...any) (sql.Result, error)
	QueryContext(ctx context.Context, query string, args ...any) (*sql.Rows, error)
	QueryRowContext(ctx context.Context, query string, args ...any) *sql.Row
	PrepareContext(ctx context.Context, query string) (*sql.Stmt, error)
}

// TestSession is a fully populated stopped session record for tests.
func TestSession(id, bundle string) core.SessionRecord {
	nativeSessionID := "native-1"
	acpSessionTitle := "Agent title"
	lastCheckpointError := "temporary recovery failure"
	return core.SessionRecord{
		Oracle:             true,
		WorkspaceID:        core.DefaultWorkspaceID,
		ID:                 id,
		Title:              "test session",
		HarnessKind:        core.HarnessKindCodex,
		LastProfile:        "codex",
		BundleID:           bundle,
		TargetTemplateID:   "local",
		ResourceAllocation: core.SessionResourceAllocationContainer{Cpus: 8, MemoryBytes: 32 * 1024 * 1024 * 1024},
		AdditionalMounts: []core.AdditionalMount{{
			Source:      "/host/cache",
			Destination: "/mnt/cache",
			Access:      core.MountAccessCow,
		}},
		State: core.SessionStateStopped,
		Target: core.TargetLocatorLocalPodman{
			ContainerID:      "container-1",
			WorkspaceStorage: core.PodmanWorkspaceLocatorContainerLayer{},
		},
		NativeSessionID:           &nativeSessionID,
		ACPSessionTitle:           &acpSessionTitle,
		CreatedAt:                 "2026-08-12T00:00:00Z",
		UpdatedAt:                 "2026-08-12T01:00:00Z",
		ViewedThroughEventOrdinal: 7,
		LastCheckpointError:       &lastCheckpointError,
		Checkpoint: &core.CheckpointMetadata{
			ArchivePath:   "sessions/test.hel.zip",
			Sha256:        strings.Repeat("a", 64),
			CreatedAt:     "2026-08-12T01:00:00Z",
			EventFrontier: 6,
		},
	}
}

// IsBusyError reports whether a failure is SQLite reporting another writer,
// which a later trigger simply retries.
func IsBusyError(err error) bool {
	var failure *sqlite.Error
	if !errors.As(err, &failure) {
		return false
	}
	// rusqlite's DatabaseBusy and DatabaseLocked cover the extended codes.
	switch failure.Code() & 0xff {
	case sqlite3.SQLITE_BUSY, sqlite3.SQLITE_LOCKED:
		return true
	}
	return false
}

// databaseMarshalJSON is serde_json::to_vec: compact, and without
// encoding/json's HTML escaping, so stored JSON matches what Rust writes.
func databaseMarshalJSON(v any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// databaseCheckFields is the part of serde's derived decoding that
// encoding/json lacks: a missing required member is an error and, with deny
// (serde's deny_unknown_fields), so is a member that is not in known.
func databaseCheckFields(data []byte, deny bool, known, required []string) error {
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return err
	}
	if deny {
		names := make([]string, 0, len(members))
		for name := range members {
			names = append(names, name)
		}
		sort.Strings(names)
		for _, name := range names {
			if !slices.Contains(known, name) {
				return fmt.Errorf("unknown field `%s`, %s", name, databaseExpected(known))
			}
		}
	}
	for _, name := range required {
		if _, ok := members[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	return nil
}

// databaseExpected is the tail of serde's unknown-field message.
func databaseExpected(known []string) string {
	quoted := make([]string, len(known))
	for i, name := range known {
		quoted[i] = "`" + name + "`"
	}
	switch len(quoted) {
	case 0:
		return "there are no fields"
	case 1:
		return "expected " + quoted[0]
	case 2:
		return "expected " + quoted[0] + " or " + quoted[1]
	}
	return "expected one of " + strings.Join(quoted, ", ")
}

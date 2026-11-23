// Port of mj-store/src/database/client_state.rs.

package store

import (
	"database/sql"
	"errors"
	"fmt"
	"math"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// clientStateRowsAffected is rusqlite's execute count.
func clientStateRowsAffected(result sql.Result, err error) (int64, error) {
	if err != nil {
		return 0, err
	}
	return result.RowsAffected()
}

// clientStateOrdinal binds an event ordinal as SQLite's signed integer,
// refusing one it cannot hold as rusqlite does.
func clientStateOrdinal(ordinal uint64) (int64, error) {
	if ordinal > math.MaxInt64 {
		return 0, fmt.Errorf("integer %d is too large to fit into an SQLite INTEGER", ordinal)
	}
	return int64(ordinal), nil
}

// clientStateNotInWorkspace is the refusal for a session outside the
// workspace a client names.
func clientStateNotInWorkspace(sessionID, workspaceID string) error {
	return fmt.Errorf("session %s is not in workspace %s", core.RustDebugString(sessionID), core.RustDebugString(workspaceID))
}

func ClientReadFrontier(clientID, workspaceID, sessionID string) (uint64, error) {
	return clientReadFrontierAt(DatabasePath(), clientID, workspaceID, sessionID)
}

func clientReadFrontierAt(path, clientID, workspaceID, sessionID string) (uint64, error) {
	connection, err := openReader(path)
	if err != nil {
		return 0, err
	}
	defer connection.Close()
	var frontier uint64
	found, err := valuesOptional(connection.QueryRow(`SELECT through_event_ordinal
               FROM client_read_frontiers
              WHERE client_id = ?1 AND workspace_id = ?2 AND session_id = ?3`, clientID, workspaceID, sessionID).Scan(&frontier))
	if err != nil {
		return 0, err
	}
	if found {
		return frontier, nil
	}
	if err := connection.QueryRow(`SELECT s.viewed_through_event_ordinal
               FROM sessions s JOIN session_contexts c USING(session_id)
              WHERE s.session_id = ?1 AND c.workspace_id = ?2`, sessionID, workspaceID).Scan(&frontier); err != nil {
		return 0, fmt.Errorf("find session %s in workspace %s: %w", core.RustDebugString(sessionID), core.RustDebugString(workspaceID), err)
	}
	return frontier, nil
}

func AdvanceClientReadFrontier(clientID, workspaceID, sessionID string, through uint64) (uint64, error) {
	return submitDatabaseWrite("advance_client_read_frontier", func(*Connection) (uint64, error) {
		return advanceClientReadFrontierAt(DatabasePath(), clientID, workspaceID, sessionID, through)
	})
}

// ClientSessionState is what this viewer has stored for this session: an
// unsent draft and how far it has read.
func ClientSessionState(clientID, workspaceID, sessionID string) (core.ClientSessionState, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return core.ClientSessionState{}, err
	}
	defer connection.Close()
	var state core.ClientSessionState
	if _, err := valuesOptional(connection.QueryRow(`SELECT draft FROM client_session_state
              WHERE client_id = ?1 AND workspace_id = ?2 AND session_id = ?3`, clientID, workspaceID, sessionID).Scan(&state.Draft)); err != nil {
		return core.ClientSessionState{}, err
	}
	if _, err := valuesOptional(connection.QueryRow(`SELECT through_event_ordinal FROM client_read_frontiers
              WHERE client_id = ?1 AND workspace_id = ?2 AND session_id = ?3`, clientID, workspaceID, sessionID).Scan(&state.ThroughEventOrdinal)); err != nil {
		return core.ClientSessionState{}, err
	}
	return state, nil
}

// PersistClientDraft stores one viewer's unsent draft.
//
// An empty draft deletes the row rather than storing emptiness, so a viewer
// that cleared its composer stops occupying a row and stops being pruned
// later for something it no longer holds.
func PersistClientDraft(clientID, workspaceID, sessionID, draft string) error {
	if strings.TrimSpace(clientID) == "" {
		return errors.New("client id is empty")
	}
	_, err := submitDatabaseWrite("persist_client_draft", func(connection *Connection) (struct{}, error) {
		transaction, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return struct{}{}, err
		}
		defer transaction.Rollback()
		if draft == "" {
			if _, err := transaction.Exec(`DELETE FROM client_session_state
                  WHERE client_id = ?1 AND workspace_id = ?2 AND session_id = ?3`, clientID, workspaceID, sessionID); err != nil {
				return struct{}{}, err
			}
			return struct{}{}, transaction.Commit()
		}
		now := workspacesNow()
		changed, err := clientStateRowsAffected(transaction.Exec(`INSERT INTO client_session_state(
                 client_id, workspace_id, session_id, draft, updated_at
             )
             SELECT ?1, ?2, ?3, ?4, ?5
              WHERE EXISTS(
                  SELECT 1 FROM session_contexts
                   WHERE session_id = ?3 AND workspace_id = ?2
              )
             ON CONFLICT(client_id, workspace_id, session_id) DO UPDATE SET
                 draft = excluded.draft,
                 updated_at = excluded.updated_at`, clientID, workspaceID, sessionID, draft, now))
		if err != nil {
			return struct{}{}, err
		}
		if changed != 1 {
			return struct{}{}, clientStateNotInWorkspace(sessionID, workspaceID)
		}
		return struct{}{}, transaction.Commit()
	})
	return err
}

// PrunePhoneClientState forgets web-viewer state that has passed its
// retention.
//
// Only rows whose client id names a phone are considered. A terminal
// client's read frontier is not the phone's to expire, and deleting one
// would lose a person's place in a conversation they are still reading.
func PrunePhoneClientState(olderThan time.Duration) (int, error) {
	cutoff := time.Now().UTC().Add(-olderThan).Format("2006-01-02T15:04:05.000Z")
	return submitDatabaseWrite("prune_phone_client_state", func(connection *Connection) (int, error) {
		transaction, err := connection.BeginWith(TransactionImmediate)
		if err != nil {
			return 0, err
		}
		defer transaction.Rollback()
		drafts, err := clientStateRowsAffected(transaction.Exec(`DELETE FROM client_session_state
              WHERE client_id LIKE 'phone:%' AND updated_at < ?1`, cutoff))
		if err != nil {
			return 0, err
		}
		frontiers, err := clientStateRowsAffected(transaction.Exec(`DELETE FROM client_read_frontiers
              WHERE client_id LIKE 'phone:%' AND updated_at < ?1`, cutoff))
		if err != nil {
			return 0, err
		}
		if err := transaction.Commit(); err != nil {
			return 0, err
		}
		return int(drafts + frontiers), nil
	})
}

func PersistReadReceipt(clientID, workspaceID, sessionID string, through uint64) (uint64, error) {
	return submitDatabaseWrite("persist_read_receipt", func(connection *Connection) (uint64, error) {
		return persistReadReceiptWith(connection, clientID, workspaceID, sessionID, through)
	})
}

func persistReadReceiptWith(conn *Connection, clientID, workspaceID, sessionID string, through uint64) (uint64, error) {
	if strings.TrimSpace(clientID) == "" {
		return 0, errors.New("client id is empty")
	}
	transaction, err := conn.BeginWith(TransactionImmediate)
	if err != nil {
		return 0, err
	}
	defer transaction.Rollback()
	var applied uint64
	found, err := valuesOptional(transaction.QueryRow("SELECT applied_event_ordinal FROM materialized_sessions WHERE session_id = ?1", sessionID).Scan(&applied))
	if err != nil {
		return 0, err
	}
	if !found {
		return 0, fmt.Errorf("unknown session %s", sessionID)
	}
	if through > applied {
		return 0, fmt.Errorf("cannot acknowledge event ordinal %d for session %s; projection is at %d", through, sessionID, applied)
	}
	stored, err := clientStateOrdinal(through)
	if err != nil {
		return 0, err
	}
	changed, err := clientStateRowsAffected(transaction.Exec(`INSERT INTO client_read_frontiers(
             client_id, workspace_id, session_id, through_event_ordinal, updated_at
         )
         SELECT ?1, ?2, ?3, ?4, ?5
          WHERE EXISTS(
              SELECT 1 FROM session_contexts
               WHERE session_id = ?3 AND workspace_id = ?2
          )
         ON CONFLICT(client_id, workspace_id, session_id) DO UPDATE SET
             through_event_ordinal = max(
                 client_read_frontiers.through_event_ordinal,
                 excluded.through_event_ordinal
             ),
             updated_at = excluded.updated_at`, clientID, workspaceID, sessionID, stored, workspacesNow()))
	if err != nil {
		return 0, err
	}
	if changed != 1 {
		return 0, clientStateNotInWorkspace(sessionID, workspaceID)
	}
	changed, err = clientStateRowsAffected(transaction.Exec(`UPDATE sessions
         SET viewed_through_event_ordinal = max(viewed_through_event_ordinal, ?2)
         WHERE session_id = ?1`, sessionID, stored))
	if err != nil {
		return 0, err
	}
	if changed != 1 {
		return 0, fmt.Errorf("unknown session %s", sessionID)
	}
	var frontier uint64
	if err := transaction.QueryRow(`SELECT through_event_ordinal
           FROM client_read_frontiers
          WHERE client_id = ?1 AND workspace_id = ?2 AND session_id = ?3`, clientID, workspaceID, sessionID).Scan(&frontier); err != nil {
		return 0, err
	}
	if err := transaction.Commit(); err != nil {
		return 0, err
	}
	return frontier, nil
}

func advanceClientReadFrontierAt(path, clientID, workspaceID, sessionID string, through uint64) (uint64, error) {
	if strings.TrimSpace(clientID) == "" {
		return 0, errors.New("client id is empty")
	}
	connection, err := open(path)
	if err != nil {
		return 0, err
	}
	stored, err := clientStateOrdinal(through)
	if err != nil {
		connection.Close()
		return 0, err
	}
	changed, err := clientStateRowsAffected(connection.Exec(`INSERT INTO client_read_frontiers(
             client_id, workspace_id, session_id, through_event_ordinal, updated_at
         )
         SELECT ?1, ?2, ?3, ?4, ?5
          WHERE EXISTS(
              SELECT 1 FROM session_contexts
               WHERE session_id = ?3 AND workspace_id = ?2
          )
         ON CONFLICT(client_id, workspace_id, session_id) DO UPDATE SET
             through_event_ordinal = max(
                 client_read_frontiers.through_event_ordinal,
                 excluded.through_event_ordinal
             ),
             updated_at = excluded.updated_at`, clientID, workspaceID, sessionID, stored, workspacesNow()))
	connection.Close()
	if err != nil {
		return 0, err
	}
	if changed != 1 {
		return 0, clientStateNotInWorkspace(sessionID, workspaceID)
	}
	return clientReadFrontierAt(path, clientID, workspaceID, sessionID)
}

// SaveTerminalDraft persists one revision of a terminal's composer while
// retiring its unchanged legacy seed on the first accepted save.
func SaveTerminalDraft(sessionID, source string, ownerPid uint32, revision uint64, draft core.DetachedSessionDraft) error {
	if revision > math.MaxInt64 {
		return errors.New("terminal draft revision exceeds SQLite range: out of range integral type conversion attempted")
	}
	stored := int64(revision)
	if stored <= 0 {
		return errors.New("terminal draft revision must be positive")
	}
	_, err := submitDatabaseWrite("save_terminal_draft", func(connection *Connection) (struct{}, error) {
		return struct{}{}, saveTerminalDraftIn(connection, sessionID, source, ownerPid, stored, &draft)
	})
	return err
}

func saveTerminalDraftIn(conn *Connection, sessionID, source string, ownerPid uint32, revision int64, draft *core.DetachedSessionDraft) error {
	if revision <= 0 {
		return errors.New("terminal draft revision must be positive")
	}
	if strings.TrimSpace(source) == "" {
		return errors.New("draft source is empty")
	}
	transaction, err := conn.BeginWith(TransactionImmediate)
	if err != nil {
		return err
	}
	defer transaction.Rollback()
	var canonicalWorkspace string
	if err := transaction.QueryRow("SELECT workspace_id FROM session_contexts WHERE session_id = ?1", sessionID).Scan(&canonicalWorkspace); err != nil {
		return fmt.Errorf("find managed session %s: %w", core.RustDebugString(sessionID), err)
	}
	var existingID string
	var storedRevision int64
	existing, err := valuesOptional(transaction.QueryRow(`SELECT draft_id, revision FROM detached_drafts
              WHERE source = ?1 AND session_id = ?2
                AND revision IS NOT NULL`, source, sessionID).Scan(&existingID, &storedRevision))
	if err != nil {
		return err
	}
	if existing && revision <= storedRevision {
		return transaction.Commit()
	}
	savedAt := workspacesNow()
	if draft.InheritedInput != nil && !existing {
		if _, err := transaction.Exec(`UPDATE sessions SET draft_input = ''
              WHERE session_id = ?1 AND draft_input = ?2`, sessionID, *draft.InheritedInput); err != nil {
			return err
		}
	}
	if existing {
		if _, err := transaction.Exec(`UPDATE detached_drafts
                SET workspace_id = ?2, owner_pid = ?3, saved_at = ?4,
                    text = ?5, revision = ?6,
                    recovered_at = CASE
                        WHEN text = ?5 THEN recovered_at
                        WHEN ?5 <> '' THEN NULL
                        ELSE recovered_at
                    END
              WHERE draft_id = ?1`, existingID, canonicalWorkspace, int64(ownerPid), savedAt, draft.Text, revision); err != nil {
			return err
		}
	} else {
		id, err := core.NewWorkspaceID()
		if err != nil {
			return err
		}
		if _, err := transaction.Exec(`INSERT INTO detached_drafts(
                 draft_id, workspace_id, session_id, source, owner_pid, saved_at, text, revision
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)`,
			id, canonicalWorkspace, sessionID, source, int64(ownerPid), savedAt, draft.Text, revision); err != nil {
			return err
		}
	}
	return transaction.Commit()
}

func SaveDetachedDraft(workspaceID string, sessionID *string, source string, ownerPid *uint32, text string) (*string, error) {
	return submitDatabaseWrite("save_detached_draft", func(*Connection) (*string, error) {
		return saveDetachedDraftAt(DatabasePath(), workspaceID, sessionID, source, ownerPid, text)
	})
}

func saveDetachedDraftAt(path, workspaceID string, sessionID *string, source string, ownerPid *uint32, text string) (*string, error) {
	if text == "" {
		return nil, nil
	}
	connection, err := open(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return insertDetachedDraft(connection, workspaceID, sessionID, source, ownerPid, text)
}

func insertDetachedDraft(conn Queryer, workspaceID string, sessionID *string, source string, ownerPid *uint32, text string) (*string, error) {
	if text == "" {
		return nil, nil
	}
	if strings.TrimSpace(source) == "" {
		return nil, errors.New("draft source is empty")
	}
	id, err := core.NewWorkspaceID()
	if err != nil {
		return nil, err
	}
	var pid any
	if ownerPid != nil {
		pid = int64(*ownerPid)
	}
	if _, err := conn.Exec(`INSERT INTO detached_drafts(
             draft_id, workspace_id, session_id, source, owner_pid, saved_at, text
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)`, id, workspaceID, sessionID, source, pid, workspacesNow(), text); err != nil {
		return nil, err
	}
	return &id, nil
}

func ListDetachedDrafts(workspaceID string) ([]core.DetachedDraft, error) {
	return listDetachedDraftsAt(DatabasePath(), workspaceID)
}

// DetachedDraftListing is a detached draft as the workspace manager lists
// it. The text stays in the store: the manager shows source and time without
// reading it.
type DetachedDraftListing struct {
	ID          string
	WorkspaceID string
	SessionID   *string
	Source      string
	OwnerPid    *uint32
	SavedAt     string
}

// ListAllDetachedDrafts is every workspace's unrecovered detached drafts,
// newest first within each workspace.
func ListAllDetachedDrafts() ([]DetachedDraftListing, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	drafts, err := valuesQueryMap(connection, `SELECT draft_id, workspace_id, session_id, source, owner_pid, saved_at
           FROM detached_drafts
          WHERE recovered_at IS NULL AND text <> ''
          ORDER BY workspace_id, saved_at DESC, draft_id DESC`, nil, func(rows *sql.Rows) (DetachedDraftListing, error) {
		var draft DetachedDraftListing
		err := rows.Scan(&draft.ID, &draft.WorkspaceID, &draft.SessionID, &draft.Source, &draft.OwnerPid, &draft.SavedAt)
		return draft, err
	})
	if err != nil {
		return nil, err
	}
	if drafts == nil {
		drafts = []DetachedDraftListing{}
	}
	return drafts, nil
}

func listDetachedDraftsAt(path, workspaceID string) ([]core.DetachedDraft, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	drafts, err := valuesQueryMap(connection, `SELECT draft_id, workspace_id, session_id, source, owner_pid, saved_at, text,
                recovered_at
           FROM detached_drafts
          WHERE workspace_id = ?1 AND recovered_at IS NULL AND text <> ''
          ORDER BY saved_at DESC, draft_id DESC`, []any{workspaceID}, func(rows *sql.Rows) (core.DetachedDraft, error) {
		var draft core.DetachedDraft
		err := rows.Scan(&draft.ID, &draft.WorkspaceID, &draft.SessionID, &draft.Source, &draft.OwnerPid, &draft.SavedAt, &draft.Text, &draft.RecoveredAt)
		return draft, err
	})
	if err != nil {
		return nil, err
	}
	if drafts == nil {
		drafts = []core.DetachedDraft{}
	}
	return drafts, nil
}

func MarkDraftRecovered(draftID string) error {
	_, err := submitDatabaseWrite("mark_draft_recovered", func(*Connection) (struct{}, error) {
		return struct{}{}, markDraftRecoveredAt(DatabasePath(), draftID)
	})
	return err
}

func markDraftRecoveredAt(path, draftID string) error {
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	changed, err := clientStateRowsAffected(connection.Exec(`UPDATE detached_drafts SET recovered_at = ?2
          WHERE draft_id = ?1 AND recovered_at IS NULL AND text <> ''`, draftID, workspacesNow()))
	if err != nil {
		return err
	}
	if changed != 1 {
		return fmt.Errorf("unknown or already recovered draft %s", core.RustDebugString(draftID))
	}
	return nil
}

// RecoverDetachedDraft explicitly restores a detached draft into its session
// composer. This is the only operation that merges client-local draft state
// back into the legacy session field, and the transaction marks the source
// draft recovered at the same durable boundary.
func RecoverDetachedDraft(draftID string) (string, error) {
	return submitDatabaseWrite("recover_detached_draft", func(*Connection) (string, error) {
		return recoverDetachedDraftAt(DatabasePath(), draftID)
	})
}

func recoverDetachedDraftAt(path, draftID string) (string, error) {
	connection, err := open(path)
	if err != nil {
		return "", err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return "", err
	}
	defer tx.Rollback()
	var sessionID *string
	var text string
	if err := tx.QueryRow(`SELECT session_id, text FROM detached_drafts
              WHERE draft_id = ?1 AND recovered_at IS NULL AND text <> ''`, draftID).Scan(&sessionID, &text); err != nil {
		return "", fmt.Errorf("find recoverable draft %s: %w", core.RustDebugString(draftID), err)
	}
	if sessionID == nil {
		return "", errors.New("draft is not associated with a session")
	}
	changed, err := clientStateRowsAffected(tx.Exec("UPDATE sessions SET draft_input = ?2 WHERE session_id = ?1", *sessionID, text))
	if err != nil {
		return "", err
	}
	if changed != 1 {
		return "", errors.New("draft session no longer exists")
	}
	if _, err := tx.Exec("UPDATE detached_drafts SET recovered_at = ?2 WHERE draft_id = ?1", draftID, workspacesNow()); err != nil {
		return "", err
	}
	if err := tx.Commit(); err != nil {
		return "", err
	}
	return *sessionID, nil
}

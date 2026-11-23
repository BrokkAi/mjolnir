// Port of mj-store/src/database/workspaces.rs.

package store

import (
	"database/sql"
	"database/sql/driver"
	"encoding/json"
	"errors"
	"fmt"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// workspacesNow is Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true).
func workspacesNow() string {
	return time.Now().UTC().Format("2006-01-02T15:04:05.000Z")
}

// workspacesRowsAffected is rusqlite's execute count.
func workspacesRowsAffected(result sql.Result, err error) (int64, error) {
	if err != nil {
		return 0, err
	}
	return result.RowsAffected()
}

// workspacesWrite runs operation in the database writer, which a store
// mutation at the default path goes through.
func workspacesWrite(label string, operation func() error) error {
	_, err := submitDatabaseWrite(label, func(*Connection) (struct{}, error) {
		return struct{}{}, operation()
	})
	return err
}

// workspacesUnknown is the refusal for a workspace the store does not have.
func workspacesUnknown(workspaceID string) error {
	return fmt.Errorf("unknown workspace %s", core.RustDebugString(workspaceID))
}

// workspacesImmediate runs body in an immediate transaction on a new
// connection to path, committing when body succeeds.
func workspacesImmediate(path string, body func(tx *Transaction) error) error {
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	tx, err := connection.BeginWith(TransactionImmediate)
	if err != nil {
		return err
	}
	defer tx.Rollback()
	if err := body(tx); err != nil {
		return err
	}
	return tx.Commit()
}

// ListWorkspaces is every workspace that can hold a session the user sees.
//
// The store keeps a row with the id `default` that older releases put
// sessions in when no workspace was named. No new session goes there, and
// the row is left out only while it holds no session at all: a store whose
// `default` holds sessions, suspended ones included, lists it as an ordinary
// workspace named `default`, so none of those sessions is hidden (launch
// finding H-3).
func ListWorkspaces() ([]core.WorkspaceRecord, error) {
	return ListWorkspacesFrom(DatabasePath())
}

// refuseLegacyDefaultName refuses the name of the hidden `default` row. The
// row keeps its name whether or not it is listed. While it holds no session
// it is not listed, so creating a workspace with its name would return a
// workspace nobody can see.
func refuseLegacyDefaultName(conn Queryer, nameKey string) error {
	var hidden bool
	if err := conn.QueryRow(`SELECT EXISTS(
             SELECT 1 FROM workspaces w
              WHERE w.workspace_id = ?1 AND w.name_key = ?2
                AND NOT EXISTS(
                    SELECT 1 FROM session_contexts c JOIN sessions s USING(session_id)
                     WHERE c.workspace_id = w.workspace_id))`, core.DefaultWorkspaceID, nameKey).Scan(&hidden); err != nil {
		return err
	}
	if hidden {
		return fmt.Errorf("the workspace name %s is reserved: it holds sessions made before Mjolnir had workspaces; choose another name", core.RustDebugString(nameKey))
	}
	return nil
}

// dbPaneSize is a pane size as the store holds it (Rust's DbPaneSize, its
// ToSql and FromSql).
type dbPaneSize struct{ size core.PaneSize }

// Value writes the stored text.
func (p dbPaneSize) Value() (driver.Value, error) {
	switch p.size {
	case core.PaneSizeMinimized:
		return "minimized", nil
	case core.PaneSizeStandard:
		return "standard", nil
	case core.PaneSizeMaximized:
		return "maximized", nil
	}
	return nil, fmt.Errorf("unknown pane size %q", string(p.size))
}

// Scan reads the stored text.
func (p *dbPaneSize) Scan(src any) error {
	var text string
	switch src := src.(type) {
	case string:
		text = src
	case []byte:
		text = string(src)
	default:
		return fmt.Errorf("Invalid column type for pane size: %T", src)
	}
	switch text {
	case "minimized":
		p.size = core.PaneSizeMinimized
	case "standard":
		p.size = core.PaneSizeStandard
	case "maximized":
		p.size = core.PaneSizeMaximized
	default:
		return fmt.Errorf("unknown pane size %q", text)
	}
	return nil
}

func LoadWorkspacePaneSizes(workspaceID string) (core.PaneSizes, error) {
	return LoadWorkspacePaneSizesFrom(DatabasePath(), workspaceID)
}

func LoadWorkspacePaneSizesFrom(path, workspaceID string) (core.PaneSizes, error) {
	connection, err := openReader(path)
	if err != nil {
		return core.PaneSizes{}, err
	}
	defer connection.Close()
	var sessions, targets, quota dbPaneSize
	found, err := valuesOptional(connection.QueryRow(`SELECT coalesce(p.sessions, 'standard'), coalesce(p.targets, 'standard'),
                    coalesce(p.quota, 'standard')
             FROM workspaces w LEFT JOIN workspace_pane_sizes p USING(workspace_id)
             WHERE w.workspace_id = ?1`, workspaceID).Scan(&sessions, &targets, &quota))
	if err != nil {
		return core.PaneSizes{}, err
	}
	if !found {
		return core.PaneSizes{}, workspacesUnknown(workspaceID)
	}
	sizes := core.PaneSizes{Sessions: sessions.size, Targets: targets.size, Quota: quota.size}
	if err := sizes.Validate(); err != nil {
		return core.PaneSizes{}, err
	}
	return sizes, nil
}

func SaveWorkspacePaneSizes(workspaceID string, sizes core.PaneSizes) error {
	return workspacesWrite("save_workspace_pane_sizes", func() error {
		return SaveWorkspacePaneSizesTo(DatabasePath(), workspaceID, sizes)
	})
}

func SaveWorkspacePaneSizesTo(path, workspaceID string, sizes core.PaneSizes) error {
	if err := sizes.Validate(); err != nil {
		return err
	}
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	if _, err := connection.Exec(`INSERT INTO workspace_pane_sizes(workspace_id, sessions, targets, quota)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(workspace_id) DO UPDATE SET
             sessions = excluded.sessions, targets = excluded.targets, quota = excluded.quota`,
		workspaceID, dbPaneSize{sizes.Sessions}, dbPaneSize{sizes.Targets}, dbPaneSize{sizes.Quota}); err != nil {
		return fmt.Errorf("save pane sizes for workspace %s: %w", core.RustDebugString(workspaceID), err)
	}
	return nil
}

func LoadWorkspaceLayout(workspaceID string) (core.ConversationLayout, error) {
	return LoadWorkspaceLayoutFrom(DatabasePath(), workspaceID)
}

func LoadWorkspaceLayoutFrom(path, workspaceID string) (core.ConversationLayout, error) {
	connection, err := openReader(path)
	if err != nil {
		return core.ConversationLayout{}, err
	}
	defer connection.Close()
	var stored *string
	found, err := valuesOptional(connection.QueryRow(`SELECT l.layout
             FROM workspaces w LEFT JOIN workspace_layouts l USING(workspace_id)
             WHERE w.workspace_id = ?1`, workspaceID).Scan(&stored))
	if err != nil {
		return core.ConversationLayout{}, err
	}
	if !found {
		return core.ConversationLayout{}, workspacesUnknown(workspaceID)
	}
	if stored == nil {
		return core.NewConversationLayout(), nil
	}
	var layout core.ConversationLayout
	if err := json.Unmarshal([]byte(*stored), &layout); err != nil {
		return core.ConversationLayout{}, fmt.Errorf("decode layout for workspace %s: %w", core.RustDebugString(workspaceID), err)
	}
	if err := layout.Validate(); err != nil {
		return core.ConversationLayout{}, err
	}
	return layout, nil
}

func SaveWorkspaceLayout(workspaceID string, layout core.ConversationLayout) error {
	return workspacesWrite("save_workspace_layout", func() error {
		return SaveWorkspaceLayoutTo(DatabasePath(), workspaceID, &layout)
	})
}

func SaveWorkspaceLayoutTo(path, workspaceID string, layout *core.ConversationLayout) error {
	if err := layout.Validate(); err != nil {
		return err
	}
	encoded, err := databaseMarshalJSON(layout)
	if err != nil {
		return err
	}
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	if _, err := connection.Exec(`INSERT INTO workspace_layouts(workspace_id, layout)
         VALUES (?1, ?2)
         ON CONFLICT(workspace_id) DO UPDATE SET layout = excluded.layout`, workspaceID, string(encoded)); err != nil {
		return fmt.Errorf("save layout for workspace %s: %w", core.RustDebugString(workspaceID), err)
	}
	return nil
}

func ListWorkspacesFrom(path string) ([]core.WorkspaceRecord, error) {
	return queryWorkspaces(path, "")
}

// WorkspaceRecord is one listed workspace, read without aggregating every
// other workspace's sessions. nil means the list would not contain it.
func WorkspaceRecord(workspaceID string) (*core.WorkspaceRecord, error) {
	records, err := queryWorkspaces(DatabasePath(), "WHERE w.workspace_id = ?1", workspaceID)
	if err != nil || len(records) == 0 {
		return nil, err
	}
	return &records[len(records)-1], nil
}

func queryWorkspaces(path string, filter string, parameters ...any) ([]core.WorkspaceRecord, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	rows, err := valuesQueryMap(connection, fmt.Sprintf(`SELECT w.workspace_id, w.name, w.created_at, w.last_opened_at,
                s.harness_kind, s.state, count(s.session_id)
          FROM workspaces w
           LEFT JOIN session_contexts c USING(workspace_id)
           LEFT JOIN sessions s USING(session_id)
          %s
          GROUP BY w.workspace_id, s.harness_kind, s.state
         HAVING w.workspace_id != 'default' OR count(s.session_id) > 0
          ORDER BY w.name_key, w.workspace_id`, filter), parameters, func(rows *sql.Rows) (core.WorkspaceRecord, error) {
		var record core.WorkspaceRecord
		var harness, state *string
		var count uint64
		if err := rows.Scan(&record.ID, &record.Name, &record.CreatedAt, &record.LastOpenedAt, &harness, &state, &count); err != nil {
			return core.WorkspaceRecord{}, err
		}
		record.SessionCount = activeSessionGroupCount(harness, state, count)
		return record, nil
	})
	if err != nil {
		return nil, err
	}
	workspaces := []core.WorkspaceRecord{}
	for _, workspace := range rows {
		if len(workspaces) > 0 && workspaces[len(workspaces)-1].ID == workspace.ID {
			workspaces[len(workspaces)-1].SessionCount += workspace.SessionCount
		} else {
			workspaces = append(workspaces, workspace)
		}
	}
	return workspaces, nil
}

// activeSessionGroupCount matches session loading: retired harness records
// are retained history, but cannot be listed or suspended and must not block
// workspace removal. It reads one (harness_kind, state, count) group whose
// harness and state may be NULL.
func activeSessionGroupCount(harness, state *string, count uint64) uint64 {
	if harness == nil || state == nil {
		return 0
	}
	if _, err := core.ParseHarnessKind(*harness); err == nil && storedSessionState(*state).IsActive() {
		return count
	}
	return 0
}

func activeWorkspaceSessionCount(conn Queryer, workspaceID string) (uint64, error) {
	counts, err := valuesQueryMap(conn, `SELECT s.harness_kind, s.state, count(*)
           FROM session_contexts c
           JOIN sessions s USING(session_id)
          WHERE c.workspace_id = ?1
          GROUP BY s.harness_kind, s.state`, []any{workspaceID}, func(rows *sql.Rows) (uint64, error) {
		var harness, state *string
		var count uint64
		if err := rows.Scan(&harness, &state, &count); err != nil {
			return 0, err
		}
		return activeSessionGroupCount(harness, state, count), nil
	})
	if err != nil {
		return 0, err
	}
	var total uint64
	for _, count := range counts {
		total += count
	}
	return total, nil
}

func CreateWorkspace(name string) (core.WorkspaceRecord, error) {
	return submitDatabaseWrite("create_workspace", func(*Connection) (core.WorkspaceRecord, error) {
		return CreateWorkspaceAt(DatabasePath(), name)
	})
}

// CreateOrGetWorkspace creates the named workspace, or returns the
// concurrently-created winner.
//
// Interactive setup uses this operation after presenting a snapshot of the
// workspace list. Several selectors can therefore submit the same normalized
// name legitimately. Explicit database creation remains strict through
// CreateWorkspace.
func CreateOrGetWorkspace(name string) (core.WorkspaceRecord, error) {
	return submitDatabaseWrite("create_or_get_workspace", func(*Connection) (core.WorkspaceRecord, error) {
		return CreateOrGetWorkspaceAt(DatabasePath(), name)
	})
}

func CreateOrGetWorkspaceAt(path, name string) (core.WorkspaceRecord, error) {
	connection, err := open(path)
	if err != nil {
		return core.WorkspaceRecord{}, err
	}
	defer connection.Close()
	transaction, err := connection.Begin()
	if err != nil {
		return core.WorkspaceRecord{}, err
	}
	defer transaction.Rollback()
	workspace, err := createOrGetWorkspaceWith(transaction, name)
	if err != nil {
		return core.WorkspaceRecord{}, err
	}
	if err := transaction.Commit(); err != nil {
		return core.WorkspaceRecord{}, err
	}
	return workspace, nil
}

func createOrGetWorkspaceWith(conn Queryer, name string) (core.WorkspaceRecord, error) {
	name, nameKey, err := core.NormalizeWorkspaceName(name)
	if err != nil {
		return core.WorkspaceRecord{}, err
	}
	id, err := core.NewWorkspaceID()
	if err != nil {
		return core.WorkspaceRecord{}, err
	}
	now := workspacesNow()
	if err := refuseLegacyDefaultName(conn, nameKey); err != nil {
		return core.WorkspaceRecord{}, err
	}
	if _, err := conn.Exec(`INSERT INTO workspaces(workspace_id, name, name_key, created_at, last_opened_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(name_key) DO NOTHING`, id, name, nameKey, now); err != nil {
		return core.WorkspaceRecord{}, fmt.Errorf("create or find workspace %s: %w", core.RustDebugString(name), err)
	}
	var workspace core.WorkspaceRecord
	if err := conn.QueryRow(`SELECT w.workspace_id, w.name, w.created_at, w.last_opened_at,
                count(s.session_id) FILTER (
                    WHERE s.state NOT IN ('stopped', 'lost', 'destroyed-with-data-loss')
                )
           FROM workspaces w
           LEFT JOIN session_contexts c USING(workspace_id)
           LEFT JOIN sessions s USING(session_id)
          WHERE w.name_key = ?1
          GROUP BY w.workspace_id`, nameKey).
		Scan(&workspace.ID, &workspace.Name, &workspace.CreatedAt, &workspace.LastOpenedAt, &workspace.SessionCount); err != nil {
		return core.WorkspaceRecord{}, err
	}
	return workspace, nil
}

func CreateWorkspaceAt(path, name string) (core.WorkspaceRecord, error) {
	name, nameKey, err := core.NormalizeWorkspaceName(name)
	if err != nil {
		return core.WorkspaceRecord{}, err
	}
	id, err := core.NewWorkspaceID()
	if err != nil {
		return core.WorkspaceRecord{}, err
	}
	now := workspacesNow()
	connection, err := open(path)
	if err != nil {
		return core.WorkspaceRecord{}, err
	}
	defer connection.Close()
	if err := refuseLegacyDefaultName(connection, nameKey); err != nil {
		return core.WorkspaceRecord{}, err
	}
	if _, err := connection.Exec(`INSERT INTO workspaces(workspace_id, name, name_key, created_at, last_opened_at)
             VALUES (?1, ?2, ?3, ?4, ?4)`, id, name, nameKey, now); err != nil {
		return core.WorkspaceRecord{}, fmt.Errorf("create workspace %s: %w", core.RustDebugString(name), err)
	}
	return core.WorkspaceRecord{ID: id, Name: name, CreatedAt: now, LastOpenedAt: now}, nil
}

func RenameWorkspace(workspaceID, name string) error {
	return workspacesWrite("rename_workspace", func() error {
		return RenameWorkspaceAt(DatabasePath(), workspaceID, name)
	})
}

func RenameWorkspaceAt(path, workspaceID, name string) error {
	name, nameKey, err := core.NormalizeWorkspaceName(name)
	if err != nil {
		return err
	}
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	changed, err := workspacesRowsAffected(connection.Exec("UPDATE workspaces SET name = ?2, name_key = ?3 WHERE workspace_id = ?1", workspaceID, name, nameKey))
	if err != nil {
		return fmt.Errorf("rename workspace to %s: %w", core.RustDebugString(name), err)
	}
	if changed != 1 {
		return workspacesUnknown(workspaceID)
	}
	return nil
}

func TouchWorkspace(workspaceID string) error {
	return workspacesWrite("touch_workspace", func() error {
		return TouchWorkspaceAt(DatabasePath(), workspaceID)
	})
}

func TouchWorkspaceAt(path, workspaceID string) error {
	connection, err := open(path)
	if err != nil {
		return err
	}
	defer connection.Close()
	changed, err := workspacesRowsAffected(connection.Exec("UPDATE workspaces SET last_opened_at = ?2 WHERE workspace_id = ?1", workspaceID, workspacesNow()))
	if err != nil {
		return err
	}
	if changed != 1 {
		return workspacesUnknown(workspaceID)
	}
	return nil
}

// DeleteWorkspace deletes a workspace that owns no active sessions or
// recoverable drafts.
//
// Inactive session records are global resume history. Their last workspace
// id is retained as historical metadata even when that workspace disappears.
func DeleteWorkspace(workspaceID string) error {
	return workspacesWrite("delete_workspace", func() error {
		return DeleteWorkspaceAt(DatabasePath(), workspaceID)
	})
}

func DeleteWorkspaceAt(path, workspaceID string) error {
	return workspacesImmediate(path, func(tx *Transaction) error {
		activeCount, err := activeWorkspaceSessionCount(tx, workspaceID)
		if err != nil {
			return err
		}
		var draftCount uint64
		if err := tx.QueryRow("SELECT count(*) FROM detached_drafts WHERE workspace_id = ?1", workspaceID).Scan(&draftCount); err != nil {
			return err
		}
		if activeCount != 0 || draftCount != 0 {
			return fmt.Errorf("workspace is not empty (%d active sessions, %d drafts)", activeCount, draftCount)
		}
		changed, err := workspacesRowsAffected(tx.Exec("DELETE FROM workspaces WHERE workspace_id = ?1", workspaceID))
		if err != nil {
			return err
		}
		if changed != 1 {
			return workspacesUnknown(workspaceID)
		}
		return nil
	})
}

// CloseWorkspace finishes a normal workspace close, retaining history but
// discarding unsent text.
func CloseWorkspace(workspaceID string) error {
	return workspacesWrite("close_workspace", func() error {
		return CloseWorkspaceAt(DatabasePath(), workspaceID)
	})
}

func CloseWorkspaceAt(path, workspaceID string) error {
	return removeInactiveWorkspaceAt(path, workspaceID, true)
}

func removeInactiveWorkspaceAt(path, workspaceID string, discardSessionDrafts bool) error {
	return workspacesImmediate(path, func(tx *Transaction) error {
		activeCount, err := activeWorkspaceSessionCount(tx, workspaceID)
		if err != nil {
			return err
		}
		if activeCount != 0 {
			return fmt.Errorf("workspace is not empty (%d active sessions remain)", activeCount)
		}
		if discardSessionDrafts {
			if _, err := tx.Exec(`UPDATE sessions SET draft_input = '' WHERE session_id IN
             (SELECT session_id FROM session_contexts WHERE workspace_id = ?1)`, workspaceID); err != nil {
				return err
			}
		}
		if _, err := tx.Exec("DELETE FROM detached_drafts WHERE workspace_id = ?1", workspaceID); err != nil {
			return err
		}
		changed, err := workspacesRowsAffected(tx.Exec("DELETE FROM workspaces WHERE workspace_id = ?1", workspaceID))
		if err != nil {
			return err
		}
		if changed != 1 {
			return workspacesUnknown(workspaceID)
		}
		return nil
	})
}

// ReassignResumableSessionWorkspace moves a durable history into the
// workspace from which it is being resumed.
//
// This is deliberately limited to states accepted by the resume controller;
// an active session must never move between live dashboards underneath its
// worker or viewers.
func ReassignResumableSessionWorkspace(sessionID, workspaceID string) error {
	return workspacesWrite("reassign_resumable_session_workspace", func() error {
		return ReassignResumableSessionWorkspaceAt(DatabasePath(), sessionID, workspaceID)
	})
}

func ReassignResumableSessionWorkspaceAt(path, sessionID, workspaceID string) error {
	return workspacesImmediate(path, func(tx *Transaction) error {
		var currentWorkspace, state string
		if err := tx.QueryRow(`SELECT c.workspace_id, s.state
               FROM session_contexts c
               JOIN sessions s USING(session_id)
              WHERE c.session_id = ?1`, sessionID).Scan(&currentWorkspace, &state); err != nil {
			return fmt.Errorf("find resumable session %s: %w", core.RustDebugString(sessionID), err)
		}
		switch storedSessionState(state) {
		case core.SessionStateStopped, core.SessionStateLost, core.SessionStateError:
		default:
			return fmt.Errorf("session %s is not resumable", sessionID)
		}
		var destinationExists bool
		if err := tx.QueryRow("SELECT EXISTS(SELECT 1 FROM workspaces WHERE workspace_id = ?1)", workspaceID).Scan(&destinationExists); err != nil {
			return err
		}
		if !destinationExists {
			return workspacesUnknown(workspaceID)
		}
		if currentWorkspace != workspaceID {
			if _, err := tx.Exec(`UPDATE detached_drafts SET workspace_id = ?2
              WHERE session_id = ?1 AND revision IS NOT NULL`, sessionID, workspaceID); err != nil {
				return err
			}
			if _, err := tx.Exec("UPDATE session_contexts SET workspace_id = ?2 WHERE session_id = ?1", sessionID, workspaceID); err != nil {
				return err
			}
		}
		return nil
	})
}

// SetSessionsWorkspace moves sessions to another workspace, all together or
// not at all.
//
// The caller passes a session and the sub-agents under it, so a family never
// splits across workspaces. This is the one path that moves a session of any
// state; the resume path above is limited to stopped sessions.
func SetSessionsWorkspace(sessionIDs []string, workspaceID string) error {
	return workspacesWrite("set_sessions_workspace", func() error {
		return SetSessionsWorkspaceAt(DatabasePath(), sessionIDs, workspaceID)
	})
}

func SetSessionsWorkspaceAt(path string, sessionIDs []string, workspaceID string) error {
	return workspacesImmediate(path, func(tx *Transaction) error {
		var destinationExists bool
		if err := tx.QueryRow("SELECT EXISTS(SELECT 1 FROM workspaces WHERE workspace_id = ?1)", workspaceID).Scan(&destinationExists); err != nil {
			return err
		}
		if !destinationExists {
			return workspacesUnknown(workspaceID)
		}
		for _, sessionID := range sessionIDs {
			if _, err := tx.Exec(`UPDATE detached_drafts SET workspace_id = ?2
              WHERE session_id = ?1 AND revision IS NOT NULL`, sessionID, workspaceID); err != nil {
				return err
			}
			changed, err := workspacesRowsAffected(tx.Exec("UPDATE session_contexts SET workspace_id = ?2 WHERE session_id = ?1", sessionID, workspaceID))
			if err != nil {
				return err
			}
			if changed != 1 {
				return fmt.Errorf("unknown session %s", sessionID)
			}
		}
		return nil
	})
}

func WorkspaceForSessionAt(path, sessionID string) (*string, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	var workspace string
	found, err := valuesOptional(connection.QueryRow("SELECT workspace_id FROM session_contexts WHERE session_id = ?1", sessionID).Scan(&workspace))
	if err != nil || !found {
		return nil, err
	}
	return &workspace, nil
}

func SessionIDsForWorkspace(workspaceID string) ([]string, error) {
	return SessionIDsForWorkspaceAt(DatabasePath(), workspaceID)
}

// SessionIDsForWorkspaceAt returns sessions whose current or last workspace
// id matches workspaceID. Callers deciding live membership must additionally
// check SessionState.
func SessionIDsForWorkspaceAt(path, workspaceID string) ([]string, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	ids, err := valuesQueryMap(connection, `SELECT c.session_id
           FROM session_contexts c
           JOIN sessions s USING(session_id)
          WHERE c.workspace_id = ?1
          ORDER BY c.created_at, c.session_id`, []any{workspaceID}, func(rows *sql.Rows) (string, error) {
		var id string
		err := rows.Scan(&id)
		return id, err
	})
	if err != nil {
		return nil, err
	}
	if ids == nil {
		ids = []string{}
	}
	return ids, nil
}

// AssignNewSessionWorkspace assigns a newly-created session context to a
// workspace. Existing contexts remain immutable here; only the guarded
// resume operation may move one.
func AssignNewSessionWorkspace(sessionID, workspaceID string) error {
	return workspacesWrite("assign_new_session_workspace", func() error {
		return AssignNewSessionWorkspaceAt(DatabasePath(), sessionID, workspaceID)
	})
}

func AssignNewSessionWorkspaceAt(path, sessionID, workspaceID string) error {
	errUnchanged := errors.New("session already in the workspace")
	err := workspacesImmediate(path, func(tx *Transaction) error {
		var current string
		if err := tx.QueryRow("SELECT workspace_id FROM session_contexts WHERE session_id = ?1", sessionID).Scan(&current); err != nil {
			return fmt.Errorf("find session context %s: %w", core.RustDebugString(sessionID), err)
		}
		if current == workspaceID {
			// Nothing to write; the transaction rolls back as Rust drops it.
			return errUnchanged
		}
		if current != core.DefaultWorkspaceID {
			return fmt.Errorf("session %s already belongs to workspace %s", sessionID, current)
		}
		var exists bool
		if err := tx.QueryRow("SELECT EXISTS(SELECT 1 FROM workspaces WHERE workspace_id = ?1)", workspaceID).Scan(&exists); err != nil {
			return err
		}
		if !exists {
			return workspacesUnknown(workspaceID)
		}
		if _, err := tx.Exec(`UPDATE detached_drafts SET workspace_id = ?2
          WHERE session_id = ?1 AND revision IS NOT NULL`, sessionID, workspaceID); err != nil {
			return err
		}
		_, err := tx.Exec("UPDATE session_contexts SET workspace_id = ?2 WHERE session_id = ?1", sessionID, workspaceID)
		return err
	})
	if errors.Is(err, errUnchanged) {
		return nil
	}
	return err
}

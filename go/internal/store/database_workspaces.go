// Port of mj-store/src/database/workspaces.rs.

package store

import (
	"database/sql/driver"
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

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
	panic("unported: mj-store/src/database/workspaces.rs refuse_legacy_default_name")
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
	panic("unported: mj-store/src/database/workspaces.rs load_workspace_pane_sizes_from")
}

func SaveWorkspacePaneSizes(workspaceID string, sizes core.PaneSizes) error {
	panic("unported: mj-store/src/database/workspaces.rs save_workspace_pane_sizes")
}

func SaveWorkspacePaneSizesTo(path, workspaceID string, sizes core.PaneSizes) error {
	panic("unported: mj-store/src/database/workspaces.rs save_workspace_pane_sizes_to")
}

func LoadWorkspaceLayout(workspaceID string) (core.ConversationLayout, error) {
	return LoadWorkspaceLayoutFrom(DatabasePath(), workspaceID)
}

func LoadWorkspaceLayoutFrom(path, workspaceID string) (core.ConversationLayout, error) {
	panic("unported: mj-store/src/database/workspaces.rs load_workspace_layout_from")
}

func SaveWorkspaceLayout(workspaceID string, layout core.ConversationLayout) error {
	panic("unported: mj-store/src/database/workspaces.rs save_workspace_layout")
}

func SaveWorkspaceLayoutTo(path, workspaceID string, layout *core.ConversationLayout) error {
	panic("unported: mj-store/src/database/workspaces.rs save_workspace_layout_to")
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
	panic("unported: mj-store/src/database/workspaces.rs query_workspaces")
}

// activeSessionGroupCount matches session loading: retired harness records
// are retained history, but cannot be listed or suspended and must not block
// workspace removal. It reads one (harness_kind, state, count) group whose
// harness and state may be NULL.
func activeSessionGroupCount(harness, state *string, count uint64) uint64 {
	panic("unported: mj-store/src/database/workspaces.rs active_session_group_count")
}

func activeWorkspaceSessionCount(conn Queryer, workspaceID string) (uint64, error) {
	panic("unported: mj-store/src/database/workspaces.rs active_workspace_session_count")
}

func CreateWorkspace(name string) (core.WorkspaceRecord, error) {
	panic("unported: mj-store/src/database/workspaces.rs create_workspace")
}

// CreateOrGetWorkspace creates the named workspace, or returns the
// concurrently-created winner.
//
// Interactive setup uses this operation after presenting a snapshot of the
// workspace list. Several selectors can therefore submit the same normalized
// name legitimately. Explicit database creation remains strict through
// CreateWorkspace.
func CreateOrGetWorkspace(name string) (core.WorkspaceRecord, error) {
	panic("unported: mj-store/src/database/workspaces.rs create_or_get_workspace")
}

func CreateOrGetWorkspaceAt(path, name string) (core.WorkspaceRecord, error) {
	panic("unported: mj-store/src/database/workspaces.rs create_or_get_workspace_at")
}

func createOrGetWorkspaceWith(conn Queryer, name string) (core.WorkspaceRecord, error) {
	panic("unported: mj-store/src/database/workspaces.rs create_or_get_workspace_with")
}

func CreateWorkspaceAt(path, name string) (core.WorkspaceRecord, error) {
	panic("unported: mj-store/src/database/workspaces.rs create_workspace_at")
}

func RenameWorkspace(workspaceID, name string) error {
	panic("unported: mj-store/src/database/workspaces.rs rename_workspace")
}

func RenameWorkspaceAt(path, workspaceID, name string) error {
	panic("unported: mj-store/src/database/workspaces.rs rename_workspace_at")
}

func TouchWorkspace(workspaceID string) error {
	panic("unported: mj-store/src/database/workspaces.rs touch_workspace")
}

func TouchWorkspaceAt(path, workspaceID string) error {
	panic("unported: mj-store/src/database/workspaces.rs touch_workspace_at")
}

// DeleteWorkspace deletes a workspace that owns no active sessions or
// recoverable drafts.
//
// Inactive session records are global resume history. Their last workspace
// id is retained as historical metadata even when that workspace disappears.
func DeleteWorkspace(workspaceID string) error {
	panic("unported: mj-store/src/database/workspaces.rs delete_workspace")
}

func DeleteWorkspaceAt(path, workspaceID string) error {
	panic("unported: mj-store/src/database/workspaces.rs delete_workspace_at")
}

// CloseWorkspace finishes a normal workspace close, retaining history but
// discarding unsent text.
func CloseWorkspace(workspaceID string) error {
	panic("unported: mj-store/src/database/workspaces.rs close_workspace")
}

func CloseWorkspaceAt(path, workspaceID string) error {
	return removeInactiveWorkspaceAt(path, workspaceID, true)
}

func removeInactiveWorkspaceAt(path, workspaceID string, discardSessionDrafts bool) error {
	panic("unported: mj-store/src/database/workspaces.rs remove_inactive_workspace_at")
}

// ReassignResumableSessionWorkspace moves a durable history into the
// workspace from which it is being resumed.
//
// This is deliberately limited to states accepted by the resume controller;
// an active session must never move between live dashboards underneath its
// worker or viewers.
func ReassignResumableSessionWorkspace(sessionID, workspaceID string) error {
	panic("unported: mj-store/src/database/workspaces.rs reassign_resumable_session_workspace")
}

func ReassignResumableSessionWorkspaceAt(path, sessionID, workspaceID string) error {
	panic("unported: mj-store/src/database/workspaces.rs reassign_resumable_session_workspace_at")
}

// SetSessionsWorkspace moves sessions to another workspace, all together or
// not at all.
//
// The caller passes a session and the sub-agents under it, so a family never
// splits across workspaces. This is the one path that moves a session of any
// state; the resume path above is limited to stopped sessions.
func SetSessionsWorkspace(sessionIDs []string, workspaceID string) error {
	panic("unported: mj-store/src/database/workspaces.rs set_sessions_workspace")
}

func SetSessionsWorkspaceAt(path string, sessionIDs []string, workspaceID string) error {
	panic("unported: mj-store/src/database/workspaces.rs set_sessions_workspace_at")
}

func WorkspaceForSessionAt(path, sessionID string) (*string, error) {
	panic("unported: mj-store/src/database/workspaces.rs workspace_for_session_at")
}

func SessionIDsForWorkspace(workspaceID string) ([]string, error) {
	return SessionIDsForWorkspaceAt(DatabasePath(), workspaceID)
}

// SessionIDsForWorkspaceAt returns sessions whose current or last workspace
// id matches workspaceID. Callers deciding live membership must additionally
// check SessionState.
func SessionIDsForWorkspaceAt(path, workspaceID string) ([]string, error) {
	panic("unported: mj-store/src/database/workspaces.rs session_ids_for_workspace_at")
}

// AssignNewSessionWorkspace assigns a newly-created session context to a
// workspace. Existing contexts remain immutable here; only the guarded
// resume operation may move one.
func AssignNewSessionWorkspace(sessionID, workspaceID string) error {
	panic("unported: mj-store/src/database/workspaces.rs assign_new_session_workspace")
}

func AssignNewSessionWorkspaceAt(path, sessionID, workspaceID string) error {
	panic("unported: mj-store/src/database/workspaces.rs assign_new_session_workspace_at")
}

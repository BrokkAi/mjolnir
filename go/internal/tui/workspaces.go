// Port of mj-tui/src/workspaces.rs.
//
// Workspace tabs and the non-blocking workspace manager.
//
// Rust's `Option<DashboardAction>` results are a DashboardAction here, nil
// being "not handled": DashboardActionNone is a handled event that asks for
// nothing, as in Rust.
package tui

import (
	"cmp"
	"slices"
	"strings"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// WorkspaceDraftEntry is a detached composer draft shown by the workspace
// manager.
type WorkspaceDraftEntry struct {
	ID string
	// SessionID is nil for a draft that belonged to no session.
	SessionID *string
	Source    string
	SavedAt   string
	// OwnerPID is nil when no process owns the draft.
	OwnerPID *uint32
}

// Equal compares two drafts field by field (Rust's derived PartialEq).
func (e *WorkspaceDraftEntry) Equal(other *WorkspaceDraftEntry) bool {
	sameSession := (e.SessionID == nil) == (other.SessionID == nil) &&
		(e.SessionID == nil || *e.SessionID == *other.SessionID)
	sameOwner := (e.OwnerPID == nil) == (other.OwnerPID == nil) &&
		(e.OwnerPID == nil || *e.OwnerPID == *other.OwnerPID)
	return e.ID == other.ID && sameSession && e.Source == other.Source && e.SavedAt == other.SavedAt && sameOwner
}

// WorkspaceManagementEntry is the workspace and its detached drafts returned
// by the controller's background management query.
type WorkspaceManagementEntry struct {
	Workspace core.WorkspaceRecord
	Drafts    []WorkspaceDraftEntry
}

// Equal compares two entries field by field (Rust's derived PartialEq).
func (e *WorkspaceManagementEntry) Equal(other *WorkspaceManagementEntry) bool {
	return e.Workspace == other.Workspace &&
		slices.EqualFunc(e.Drafts, other.Drafts, func(a, b WorkspaceDraftEntry) bool { return a.Equal(&b) })
}

// Clone copies the entry and its drafts (Rust's derived Clone).
func (e *WorkspaceManagementEntry) Clone() WorkspaceManagementEntry {
	return WorkspaceManagementEntry{Workspace: e.Workspace, Drafts: slices.Clone(e.Drafts)}
}

// WorkspaceControlFocus is the transient focus inside the workspace pane:
// its tabs or the pinned manager button.
type WorkspaceControlFocus int

const (
	WorkspaceControlFocusTabs WorkspaceControlFocus = iota
	WorkspaceControlFocusMenu
)

// WorkspaceManagerView is the page the workspace manager shows. The
// variants are comparable, so == is Rust's derived PartialEq; GoString is
// Rust's Debug, which the draft list uses as its identity.
type WorkspaceManagerView interface {
	isWorkspaceManagerView()
	GoString() string
}

// WorkspaceManagerViewList is the list of workspaces.
type WorkspaceManagerViewList struct{}

// WorkspaceManagerViewCreate names a new workspace.
type WorkspaceManagerViewCreate struct{}

// WorkspaceManagerViewRename renames a workspace.
type WorkspaceManagerViewRename struct {
	WorkspaceID string
}

// WorkspaceManagerViewClose confirms deleting a workspace.
type WorkspaceManagerViewClose struct {
	WorkspaceID   string
	WorkspaceName string
	SessionCount  uint64
	DraftCount    int
}

// WorkspaceManagerViewDrafts lists a workspace's detached drafts.
type WorkspaceManagerViewDrafts struct {
	WorkspaceID string
}

func (WorkspaceManagerViewList) isWorkspaceManagerView()   {}
func (WorkspaceManagerViewCreate) isWorkspaceManagerView() {}
func (WorkspaceManagerViewRename) isWorkspaceManagerView() {}
func (WorkspaceManagerViewClose) isWorkspaceManagerView()  {}
func (WorkspaceManagerViewDrafts) isWorkspaceManagerView() {}

func (WorkspaceManagerViewList) GoString() string   { return "List" }
func (WorkspaceManagerViewCreate) GoString() string { return "Create" }

func (v WorkspaceManagerViewRename) GoString() string {
	panic("unported: mj-tui/src/workspaces.rs WorkspaceManagerView Debug")
}

func (v WorkspaceManagerViewClose) GoString() string {
	panic("unported: mj-tui/src/workspaces.rs WorkspaceManagerView Debug")
}

func (v WorkspaceManagerViewDrafts) GoString() string {
	panic("unported: mj-tui/src/workspaces.rs WorkspaceManagerView Debug")
}

// WorkspaceControl identifies the workspace manager's controls.
type WorkspaceControl int

const (
	WorkspaceControlList WorkspaceControl = iota
	WorkspaceControlDraftList
	WorkspaceControlNew
	WorkspaceControlOpen
	WorkspaceControlRename
	WorkspaceControlSave
	WorkspaceControlDelete
	WorkspaceControlConfirmClose
	WorkspaceControlCancelClose
	WorkspaceControlContinueWorking
	WorkspaceControlDrafts
	WorkspaceControlName
	WorkspaceControlCreate
	WorkspaceControlRecover
	WorkspaceControlCancel
	WorkspaceControlBack
)

// WorkspaceMutation is the background operation the manager is waiting on.
// The values are the Rust variant names, which is how the busy row prints
// them (Rust's Debug).
type WorkspaceMutation string

const (
	WorkspaceMutationClose   WorkspaceMutation = "Close"
	WorkspaceMutationLoad    WorkspaceMutation = "Load"
	WorkspaceMutationCreate  WorkspaceMutation = "Create"
	WorkspaceMutationRename  WorkspaceMutation = "Rename"
	WorkspaceMutationRecover WorkspaceMutation = "Recover"
)

// WorkspaceManager is the state for the workspace manager. It only stores a
// snapshot and drafts; all filesystem/database work is requested through
// [DashboardAction].
//
// Rust's RefCell form is a plain field mutated through the manager's
// pointer.
type WorkspaceManager struct {
	generation        uint64
	activeWorkspaceID *string
	entries           []WorkspaceManagementEntry
	selected          int
	selectedDraft     int
	view              WorkspaceManagerView
	// pendingCommand is the rename or delete the shortcut asked for before
	// the first snapshot arrived; nil when there is none.
	pendingCommand *pendingWorkspaceCommand
	// openedByShortcut says the Rename or Close view was opened by its
	// shortcut from the dashboard, not from the list, so leaving it returns
	// to the dashboard.
	openedByShortcut  bool
	closingWorkspaces map[string]struct{}
	name              chat.TextInput
	form              chat.Dialog[WorkspaceControl]
	loading           bool
	// busy is the mutation in flight; nil when there is none.
	busy    *WorkspaceMutation
	error   *string
	success *string
}

// pendingWorkspaceCommand is Rust's `(workspace_id, close)` pair.
type pendingWorkspaceCommand struct {
	workspaceID string
	close       bool
}

func managerForm() chat.Dialog[WorkspaceControl] {
	return chat.NewDialog[WorkspaceControl]()
}

func (m *WorkspaceManager) prepareDialogState() {
	panic("unported: mj-tui/src/workspaces.rs WorkspaceManager::prepare_dialog_state")
}

// workspaceManagerLoading is Rust's `WorkspaceManager::loading`: a manager
// waiting for its first snapshot.
func workspaceManagerLoading(generation uint64, activeWorkspaceID *string) WorkspaceManager {
	busy := WorkspaceMutationLoad
	return WorkspaceManager{
		generation:        generation,
		activeWorkspaceID: activeWorkspaceID,
		view:              WorkspaceManagerViewList{},
		closingWorkspaces: map[string]struct{}{},
		name:              chat.NewTextInput(),
		form:              managerForm(),
		loading:           true,
		busy:              &busy,
	}
}

func (m *WorkspaceManager) selectedEntry() *WorkspaceManagementEntry {
	if m.selected < 0 || m.selected >= len(m.entries) {
		return nil
	}
	return &m.entries[m.selected]
}

// selectIndex is Rust's `WorkspaceManager::select`, renamed because select
// is a Go keyword.
func (m *WorkspaceManager) selectIndex(index int) {
	m.selected = min(index, max(len(m.entries)-1, 0))
	m.selectedDraft = 0
	if _, list := m.view.(WorkspaceManagerViewList); list {
		if entry := m.selectedEntry(); entry != nil {
			m.name = chat.TextInputFromValue(entry.Workspace.Name).WithMaxChars(64)
		}
	}
}

func (m *WorkspaceManager) viewWorkspaceID() (string, bool) {
	switch view := m.view.(type) {
	case WorkspaceManagerViewRename:
		return view.WorkspaceID, true
	case WorkspaceManagerViewClose:
		return view.WorkspaceID, true
	case WorkspaceManagerViewDrafts:
		return view.WorkspaceID, true
	default:
		return "", false
	}
}

func (m *WorkspaceManager) viewedEntry() *WorkspaceManagementEntry {
	workspaceID, ok := m.viewWorkspaceID()
	if !ok {
		return nil
	}
	for index := range m.entries {
		if m.entries[index].Workspace.ID == workspaceID {
			return &m.entries[index]
		}
	}
	return nil
}

func (m *WorkspaceManager) openSelectedCommand(close bool) {
	panic("unported: mj-tui/src/workspaces.rs WorkspaceManager::open_selected_command")
}

func (m *WorkspaceManager) setError(message string) {
	m.loading = false
	m.busy = nil
	m.error = &message
	m.success = nil
}

func (m *WorkspaceManager) canMutate() bool {
	return !m.loading && m.busy == nil
}

// leavesToDashboard reports whether leaving the current view should close
// the manager: the view was opened by its shortcut, so the list is not
// where the user was.
func (m *WorkspaceManager) leavesToDashboard() bool {
	switch m.view.(type) {
	case WorkspaceManagerViewRename, WorkspaceManagerViewClose:
		return m.openedByShortcut
	default:
		return false
	}
}

func (m *WorkspaceManager) resetToList() {
	m.openedByShortcut = false
	m.view = WorkspaceManagerViewList{}
	m.selectedDraft = 0
	m.error = nil
	m.success = nil
	if entry := m.selectedEntry(); entry != nil {
		m.name = chat.TextInputFromValue(entry.Workspace.Name).WithMaxChars(64)
	}
	m.form.Focus(WorkspaceControlList)
}

func (m *WorkspaceManager) actions() []chat.ButtonSpec[WorkspaceControl] {
	panic("unported: mj-tui/src/workspaces.rs WorkspaceManager::actions")
}

func (m *WorkspaceManager) syncForm() {
	panic("unported: mj-tui/src/workspaces.rs WorkspaceManager::sync_form")
}

// Clone copies the manager, its entries and its form (Rust's derived
// Clone).
func (m *WorkspaceManager) Clone() WorkspaceManager {
	panic("unported: mj-tui/src/workspaces.rs WorkspaceManager Clone")
}

// Equal compares two managers field by field (Rust's derived PartialEq).
func (m *WorkspaceManager) Equal(other *WorkspaceManager) bool {
	panic("unported: mj-tui/src/workspaces.rs WorkspaceManager PartialEq")
}

// workspaceManagerGeneration is the open manager's generation, if the mode
// is the workspace manager.
func workspaceManagerGeneration(mode Mode) (uint64, bool) {
	panic("unported: mj-tui/src/workspaces.rs workspace_manager_generation")
}

func workspaceManagerRef(mode Mode) *WorkspaceManager {
	panic("unported: mj-tui/src/workspaces.rs workspace_manager_ref")
}

func workspaceManagerInMode(mode *Mode) *WorkspaceManager {
	panic("unported: mj-tui/src/workspaces.rs workspace_manager_in_mode")
}

// sortWorkspaceEntries lists workspaces by name, whatever order the daemon
// keeps.
func sortWorkspaceEntries(entries []WorkspaceManagementEntry) {
	slices.SortStableFunc(entries, func(a, b WorkspaceManagementEntry) int {
		return cmp.Or(
			strings.Compare(strings.ToLower(a.Workspace.Name), strings.ToLower(b.Workspace.Name)),
			strings.Compare(a.Workspace.Name, b.Workspace.Name),
			strings.Compare(a.Workspace.ID, b.Workspace.ID),
		)
	})
}

// handleWorkspacePaneKey handles the two keyboard stops inside the
// workspace pane. Tabs own left/right selection (with up/down as vertical
// mirrors of the same moves); the pinned menu is a second local stop before
// the ordinary dashboard Tab ring continues to Sessions. It returns nil when
// the key is not the pane's.
func (d *DashboardState) handleWorkspacePaneKey(key tea.KeyMsg) DashboardAction {
	panic("unported: mj-tui/src/workspaces.rs DashboardState::handle_workspace_pane_key")
}

func (d *DashboardState) scrollWorkspaces(delta int) {
	panic("unported: mj-tui/src/workspaces.rs DashboardState::scroll_workspaces")
}

func (d *DashboardState) selectAdjacentWorkspace(delta int) DashboardAction {
	panic("unported: mj-tui/src/workspaces.rs DashboardState::select_adjacent_workspace")
}

// SelectWorkspaceIndex selects the workspace in slot index, counting from
// zero, or does nothing when there is no tab there. This is the
// numbered-key sibling of [DashboardState.selectAdjacentWorkspace].
func (d *DashboardState) SelectWorkspaceIndex(index int) DashboardAction {
	panic("unported: mj-tui/src/workspaces.rs DashboardState::select_workspace_index")
}

// beginWorkspaceCommand opens the manager and requests its first snapshot
// off the UI loop.
func (d *DashboardState) beginWorkspaceCommand(close bool) DashboardAction {
	panic("unported: mj-tui/src/workspaces.rs DashboardState::begin_workspace_command")
}

// BeginWorkspaceManager opens the workspace manager and asks for its first
// snapshot.
func (d *DashboardState) BeginWorkspaceManager() DashboardAction {
	panic("unported: mj-tui/src/workspaces.rs DashboardState::begin_workspace_manager")
}

// WorkspaceEntriesWithout is the visible manager's entries once the daemon
// has confirmed deleting workspaceID, so the dialog does not wait for a
// second query to learn what the confirmation already says. Empty when no
// manager is open.
func (d *DashboardState) WorkspaceEntriesWithout(workspaceID string) []WorkspaceManagementEntry {
	panic("unported: mj-tui/src/workspaces.rs DashboardState::workspace_entries_without")
}

// WorkspaceCloseFinished clears operation state even when its original
// dialog has been dismissed. It returns the generation of a reopened
// progress dialog that owns this operation's result too.
func (d *DashboardState) WorkspaceCloseFinished(workspaceID string) (uint64, bool) {
	panic("unported: mj-tui/src/workspaces.rs DashboardState::workspace_close_finished")
}

// WorkspaceManagementIsCurrent reports whether a background workspace query
// or mutation may still update the visible manager. This has the same mode
// and generation guard as [DashboardState.FinishWorkspaceManagement].
func (d *DashboardState) WorkspaceManagementIsCurrent(generation uint64) bool {
	panic("unported: mj-tui/src/workspaces.rs DashboardState::workspace_management_is_current")
}

// FinishWorkspaceManagement installs a manager snapshot, ignoring replies
// from an older modal or mutation. Rust's `Result<Vec<_>, String>` is
// entries and err: a non-nil err is the failure, whose text the responsive
// modal shows. A detailsPending answer carries the workspace list ahead of
// the details query: the list shows at once, while loading stays on so
// mutations (and the shortcut's pending command) wait for the complete
// answer — closing from a list that has not seen its drafts could otherwise
// skip the confirmation that names them.
func (d *DashboardState) FinishWorkspaceManagement(generation uint64, entries []WorkspaceManagementEntry, err error, detailsPending bool) WorkspaceManagementOutcome {
	panic("unported: mj-tui/src/workspaces.rs DashboardState::finish_workspace_management")
}

func (d *DashboardState) handleWorkspaceManagerEvent(event tea.Msg) DashboardAction {
	panic("unported: mj-tui/src/workspaces.rs DashboardState::handle_workspace_manager_event")
}

func (d *DashboardState) workspaceManagerMutation(mutation WorkspaceMutation) DashboardAction {
	panic("unported: mj-tui/src/workspaces.rs DashboardState::workspace_manager_mutation")
}

// closeNeedsConfirmation reports whether deleting this workspace discards
// something the user cannot get back. SessionCount is the daemon's own count
// of the active sessions it refuses to delete under; detached drafts are
// discarded by a delete.
func closeNeedsConfirmation(entry *WorkspaceManagementEntry) bool {
	return entry.Workspace.SessionCount > 0 || len(entry.Drafts) > 0
}

// WorkspaceManagementOutcome is what a workspace-management result asks of
// the caller.
type WorkspaceManagementOutcome struct {
	// Foreground says whether the result belongs to the visible manager.
	Foreground bool
	// Action is follow-up work the result started, such as deleting an
	// empty workspace the user asked to delete before the manager had
	// loaded.
	Action DashboardAction
}

func workspaceManagementOutcomeIgnored() WorkspaceManagementOutcome {
	panic("unported: mj-tui/src/workspaces.rs WorkspaceManagementOutcome::ignored")
}

// workspaceTabClick handles clicks on the workspace pane before row and
// pane hitboxes. It returns nil when the click is not the pane's.
func workspaceTabClick(dashboard *DashboardState, column, row int) DashboardAction {
	panic("unported: mj-tui/src/workspaces.rs workspace_tab_click")
}

const workspacesTitle = " Workspaces "

// borderCornerCells is the two rounded corners a bordered title row cannot
// draw into.
const borderCornerCells = 2

const (
	workspaceMaxRows   = 7
	workspaceMenuWidth = 3
)

// WorkspaceTabs is the measured layout of the workspace tab rows.
type WorkspaceTabs struct {
	ids    []string
	labels []string
	// badges holds each workspace's attention badge; nil when it has none.
	badges []*chat.Span
	cells  []workspaceTabCell
	rows   int
}

// workspaceTabCell is one Rust `(workspace index, content row, column, cell
// width)` tuple.
type workspaceTabCell struct {
	index int
	row   int
	x     int
	width int
}

func newWorkspaceTabs(dashboard *DashboardState, width int) WorkspaceTabs {
	panic("unported: mj-tui/src/workspaces.rs WorkspaceTabs::new")
}

func workspaceTabsFitting(dashboard *DashboardState, width int, visible int) WorkspaceTabs {
	tabs := newWorkspaceTabs(dashboard, width)
	if tabs.rows > visible {
		return newWorkspaceTabs(dashboard, max(width-1, 0))
	}
	return tabs
}

func workspacePaneHeight(dashboard *DashboardState, width int) int {
	panic("unported: mj-tui/src/workspaces.rs workspace_pane_height")
}

// renderWorkspaceTabs draws the workspace rows using the same cell layout
// used to measure them.
func renderWorkspaceTabs(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState) {
	panic("unported: mj-tui/src/workspaces.rs render_workspace_tabs")
}

// workspaceTabLine is one tab: the name, then the attention badge when there
// is one and the tab has room for the name (or at least three cells of a
// longer name). The badge keeps its own colour on an unselected tab; a
// selected tab is already the highlight colour, and the counts stay legible
// in it.
func workspaceTabLine(name string, badge *chat.Span, width int, style chat.Style) chat.Line {
	panic("unported: mj-tui/src/workspaces.rs workspace_tab_line")
}

func workspaceBreadcrumb(segments []string, width int, reviewers bool) string {
	panic("unported: mj-tui/src/workspaces.rs workspace_breadcrumb")
}

func workspaceLabel(name string, width int) string {
	panic("unported: mj-tui/src/workspaces.rs workspace_label")
}

// renderWorkspaceManager is the standard Dialog modal renderer used by the
// parent combined renderer.
func renderWorkspaceManager(frame *chat.Frame, area uv.Rectangle, dialog *WorkspaceManager, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/workspaces.rs render_workspace_manager")
}

func renderManagerList(frame *chat.Frame, columns chat.ColumnSplit, dialog *WorkspaceManager, form *chat.Dialog[WorkspaceControl]) {
	panic("unported: mj-tui/src/workspaces.rs render_manager_list")
}

func renderManagerNameView(frame *chat.Frame, columns chat.ColumnSplit, dialog *WorkspaceManager, form *chat.Dialog[WorkspaceControl], label string, submit WorkspaceControl, submitLabel string) {
	panic("unported: mj-tui/src/workspaces.rs render_manager_name_view")
}

func renderManagerDrafts(frame *chat.Frame, columns chat.ColumnSplit, dialog *WorkspaceManager, form *chat.Dialog[WorkspaceControl]) {
	panic("unported: mj-tui/src/workspaces.rs render_manager_drafts")
}

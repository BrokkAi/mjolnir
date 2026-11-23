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
	"fmt"
	"maps"
	"slices"
	"strings"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
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
	return fmt.Sprintf("Rename { workspace_id: %s }", core.RustDebugString(v.WorkspaceID))
}

func (v WorkspaceManagerViewClose) GoString() string {
	return fmt.Sprintf(
		"Close { workspace_id: %s, workspace_name: %s, session_count: %d, draft_count: %d }",
		core.RustDebugString(v.WorkspaceID), core.RustDebugString(v.WorkspaceName), v.SessionCount, v.DraftCount,
	)
}

func (v WorkspaceManagerViewDrafts) GoString() string {
	return fmt.Sprintf("Drafts { workspace_id: %s }", core.RustDebugString(v.WorkspaceID))
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
	m.syncForm()
	form := &m.form
	form.SetDismissalEnabled(m.busy == nil || *m.busy == WorkspaceMutationLoad || *m.busy == WorkspaceMutationClose)
	form.SetActionRole(WorkspaceControlCancel, chat.ActionRoleCancel)
	form.SetActionRole(WorkspaceControlBack, chat.ActionRoleBack)
	switch m.view.(type) {
	case WorkspaceManagerViewCreate, WorkspaceManagerViewRename:
		form.TrackDraft([]string{m.name.String()})
		form.SetDismissActions([]WorkspaceControl{WorkspaceControlCancel, WorkspaceControlBack})
		primary := WorkspaceControlSave
		if _, create := m.view.(WorkspaceManagerViewCreate); create {
			primary = WorkspaceControlCreate
		}
		form.SetDefaultAction(primary)
		form.SetSubmit(WorkspaceControlName, primary)
	default:
		form.ResetDraft()
		form.SetDismissActions([]WorkspaceControl{WorkspaceControlCancel})
	}
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
	entry := m.selectedEntry()
	if entry == nil {
		return
	}
	m.openedByShortcut = false
	m.name = chat.TextInputFromValue(entry.Workspace.Name).WithMaxChars(64)
	if close {
		m.view = WorkspaceManagerViewClose{
			WorkspaceID:   entry.Workspace.ID,
			WorkspaceName: entry.Workspace.Name,
			SessionCount:  entry.Workspace.SessionCount,
			DraftCount:    len(entry.Drafts),
		}
	} else {
		m.view = WorkspaceManagerViewRename{WorkspaceID: entry.Workspace.ID}
	}
	if close {
		if id, ok := m.viewWorkspaceID(); ok {
			if _, closing := m.closingWorkspaces[id]; closing {
				busy := WorkspaceMutationClose
				m.busy = &busy
			}
		}
	}
	m.error = nil
	m.success = nil
	if close {
		m.form.Focus(WorkspaceControlCancel)
	} else {
		m.form.Focus(WorkspaceControlName)
	}
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
	ready := m.canMutate()
	dismiss := m.busy == nil
	button := func(id WorkspaceControl, label string, enabled bool) chat.ButtonSpec[WorkspaceControl] {
		return chat.ButtonSpec[WorkspaceControl]{ID: id, Label: label, Enabled: enabled}
	}
	switch m.view.(type) {
	case WorkspaceManagerViewList:
		selected := m.selectedEntry()
		actions := []chat.ButtonSpec[WorkspaceControl]{
			button(WorkspaceControlNew, "New workspace", ready),
			button(WorkspaceControlRename, "Rename", ready && selected != nil),
			button(WorkspaceControlDelete, "Delete…", ready && selected != nil),
		}
		if selected != nil && len(selected.Drafts) > 0 {
			actions = append(actions, button(WorkspaceControlDrafts, "Drafts", ready))
		}
		// Opening a workspace needs no details: it stays available while
		// the details load runs or failed, but not while a mutation is in
		// flight.
		openable := selected != nil && (m.busy == nil || *m.busy == WorkspaceMutationLoad)
		return append(actions, button(WorkspaceControlOpen, "Open", openable))
	case WorkspaceManagerViewCreate:
		return []chat.ButtonSpec[WorkspaceControl]{
			button(WorkspaceControlCancel, "Cancel", dismiss),
			button(WorkspaceControlBack, "Back", dismiss),
			button(WorkspaceControlCreate, "Create", ready),
		}
	case WorkspaceManagerViewRename:
		return []chat.ButtonSpec[WorkspaceControl]{
			button(WorkspaceControlCancel, "Cancel", dismiss),
			button(WorkspaceControlBack, "Back", dismiss),
			button(WorkspaceControlSave, "Save", ready),
		}
	case WorkspaceManagerViewClose:
		if m.busy != nil && *m.busy == WorkspaceMutationClose {
			return []chat.ButtonSpec[WorkspaceControl]{
				button(WorkspaceControlContinueWorking, "Continue working", true),
				button(WorkspaceControlCancelClose, "Cancel deletion", true),
			}
		}
		return []chat.ButtonSpec[WorkspaceControl]{
			button(WorkspaceControlCancel, "Cancel", dismiss),
			button(WorkspaceControlConfirmClose, "Delete workspace", ready),
		}
	default:
		viewed := m.viewedEntry()
		return []chat.ButtonSpec[WorkspaceControl]{
			button(WorkspaceControlBack, "Back", dismiss),
			button(WorkspaceControlRecover, "Recover", ready && viewed != nil && len(viewed.Drafts) > 0),
		}
	}
}

func (m *WorkspaceManager) syncForm() {
	actions := m.actions()
	draftLen := 0
	if entry := m.viewedEntry(); entry != nil {
		draftLen = len(entry.Drafts)
	}
	form := &m.form
	form.BeginUpdate()
	var initial WorkspaceControl
	switch m.view.(type) {
	case WorkspaceManagerViewList:
		form.DeclareWithEnabled(
			WorkspaceControlList,
			chat.ControlKindChoiceList{Len: len(m.entries), Selected: m.selected},
			len(m.entries) > 0,
		)
		if len(m.entries) == 0 {
			initial = WorkspaceControlNew
		} else {
			initial = WorkspaceControlList
		}
	case WorkspaceManagerViewCreate, WorkspaceManagerViewRename:
		form.Declare(WorkspaceControlName, chat.ControlKindTextField{})
		initial = WorkspaceControlName
	case WorkspaceManagerViewClose:
		initial = WorkspaceControlCancel
	default:
		form.DeclareWithEnabled(
			WorkspaceControlDraftList,
			chat.ControlKindChoiceList{Len: draftLen, Selected: m.selectedDraft},
			draftLen > 0,
		)
		form.SetListIdentity(WorkspaceControlDraftList, m.view.GoString())
		initial = WorkspaceControlDraftList
	}
	declared := make([]chat.DialogAction[WorkspaceControl], 0, len(actions))
	for _, action := range actions {
		var role chat.ActionRole
		switch action.ID {
		case WorkspaceControlCancel:
			role = chat.ActionRoleCancel
		case WorkspaceControlBack:
			role = chat.ActionRoleBack
		case WorkspaceControlOpen, WorkspaceControlCreate, WorkspaceControlSave,
			WorkspaceControlConfirmClose, WorkspaceControlRecover:
			role = chat.ActionRolePrimary
		default:
			role = chat.ActionRoleSecondary
		}
		declared = append(declared, chat.DialogAction[WorkspaceControl]{
			ID:      action.ID,
			Label:   action.Label,
			Enabled: action.Enabled,
			Role:    role,
		})
	}
	form.DeclareActions(declared)
	form.EndFrame(initial)
}

// Clone copies the manager, its entries and its form (Rust's derived
// Clone).
func (m *WorkspaceManager) Clone() WorkspaceManager {
	clone := *m
	clone.activeWorkspaceID = workspacesCloneString(m.activeWorkspaceID)
	clone.entries = make([]WorkspaceManagementEntry, len(m.entries))
	for index := range m.entries {
		clone.entries[index] = m.entries[index].Clone()
	}
	if m.pendingCommand != nil {
		pending := *m.pendingCommand
		clone.pendingCommand = &pending
	}
	clone.closingWorkspaces = maps.Clone(m.closingWorkspaces)
	clone.form = m.form.Clone()
	if m.busy != nil {
		busy := *m.busy
		clone.busy = &busy
	}
	clone.error = workspacesCloneString(m.error)
	clone.success = workspacesCloneString(m.success)
	return clone
}

// Equal compares two managers field by field (Rust's derived PartialEq).
func (m *WorkspaceManager) Equal(other *WorkspaceManager) bool {
	samePending := (m.pendingCommand == nil) == (other.pendingCommand == nil) &&
		(m.pendingCommand == nil || *m.pendingCommand == *other.pendingCommand)
	sameBusy := (m.busy == nil) == (other.busy == nil) && (m.busy == nil || *m.busy == *other.busy)
	return m.generation == other.generation &&
		workspacesStringsEqual(m.activeWorkspaceID, other.activeWorkspaceID) &&
		slices.EqualFunc(m.entries, other.entries, func(a, b WorkspaceManagementEntry) bool { return a.Equal(&b) }) &&
		m.selected == other.selected &&
		m.selectedDraft == other.selectedDraft &&
		m.view == other.view &&
		samePending &&
		m.openedByShortcut == other.openedByShortcut &&
		maps.Equal(m.closingWorkspaces, other.closingWorkspaces) &&
		m.name.Equal(&other.name) &&
		m.form.Equal(&other.form) &&
		m.loading == other.loading &&
		sameBusy &&
		workspacesStringsEqual(m.error, other.error) &&
		workspacesStringsEqual(m.success, other.success)
}

// workspacesCloneString copies an optional string.
func workspacesCloneString(value *string) *string {
	if value == nil {
		return nil
	}
	clone := *value
	return &clone
}

// workspacesStringsEqual compares two optional strings as Rust compares
// `Option<String>`.
func workspacesStringsEqual(a, b *string) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
}

// workspaceManagerGeneration is the open manager's generation, if the mode
// is the workspace manager.
func workspaceManagerGeneration(mode Mode) (uint64, bool) {
	if manager, ok := mode.(ModeWorkspaceManager); ok {
		return manager.generation, true
	}
	return 0, false
}

func workspaceManagerRef(mode Mode) *WorkspaceManager {
	if manager, ok := mode.(ModeWorkspaceManager); ok {
		return manager.WorkspaceManager
	}
	return nil
}

func workspaceManagerInMode(mode *Mode) *WorkspaceManager {
	return workspaceManagerRef(*mode)
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
	press := tea.KeyPressMsg(key.Key())
	if d.focus != FocusWorkspaces || press.Mod&(tea.ModCtrl|tea.ModAlt|tea.ModSuper) != 0 {
		return nil
	}
	code := chat.KeyPressCode(press)
	if _, ok := d.SubagentParentID(); ok && (code == tea.KeyEnter || code == ' ' || code == tea.KeyEscape) {
		d.recordEventHandled()
		return DashboardActionExitSubagentWorkspace{}
	}
	if d.workspaceDropdownArea != nil &&
		d.workspaceControlFocus == WorkspaceControlFocusTabs &&
		(code == tea.KeyEnter || code == ' ') {
		d.beginWorkspaceDropdown()
		d.recordEventHandled()
		return DashboardActionNone{}
	}
	// The tab row is horizontal, so Up/Down carry the same meaning as
	// Left/Right everywhere in this pane.
	switch code {
	case tea.KeyUp:
		code = tea.KeyLeft
	case tea.KeyDown:
		code = tea.KeyRight
	}
	// Bubble Tea reports crossterm's BackTab as Tab with Shift.
	backTab := code == tea.KeyTab && press.Mod.Contains(tea.ModShift)
	tabs := d.workspaceControlFocus == WorkspaceControlFocusTabs
	menu := d.workspaceControlFocus == WorkspaceControlFocusMenu
	switch {
	case tabs && code == tea.KeyTab && !backTab:
		d.workspaceControlFocus = WorkspaceControlFocusMenu
		d.surfaceForm.Focus(SurfaceControlWorkspaceMenu{})
		d.recordEventHandled()
		return DashboardActionNone{}
	case menu && code == tea.KeyTab && !backTab:
		d.CycleFocus(false)
		d.recordEventHandled()
		return DashboardActionNone{}
	case menu && backTab:
		d.workspaceControlFocus = WorkspaceControlFocusTabs
		d.recordEventHandled()
		return DashboardActionNone{}
	case tabs && backTab:
		d.CycleFocus(true)
		d.recordEventHandled()
		return DashboardActionNone{}
	case menu && code == tea.KeyLeft:
		d.workspaceControlFocus = WorkspaceControlFocusTabs
		d.recordEventHandled()
		return DashboardActionNone{}
	case menu && (code == tea.KeyEnter || code == ' '):
		d.recordEventHandled()
		return d.DispatchCommand(CommandIdWorkspaces)
	case tabs && code == tea.KeyRight:
		ids := d.workspaceIDs()
		atEnd := false
		if active, ok := d.ActiveWorkspaceID(); ok {
			if index := slices.Index(ids, active); index >= 0 {
				atEnd = index+1 >= len(ids)
			}
		}
		if atEnd {
			d.workspaceControlFocus = WorkspaceControlFocusMenu
			d.surfaceForm.Focus(SurfaceControlWorkspaceMenu{})
			d.recordEventHandled()
			return DashboardActionNone{}
		}
		d.recordEventHandled()
		return d.selectAdjacentWorkspace(1)
	case tabs && code == tea.KeyLeft:
		d.recordEventHandled()
		return d.selectAdjacentWorkspace(-1)
	default:
		return nil
	}
}

func (d *DashboardState) scrollWorkspaces(delta int) {
	if geometry := d.workspaceScrollGeometry; geometry != nil {
		rows, visible := geometry.First, geometry.Second
		d.workspaceScroll = min(max(d.workspaceScroll+delta, 0), max(rows-visible, 0))
	}
}

func (d *DashboardState) selectAdjacentWorkspace(delta int) DashboardAction {
	ids := d.workspaceIDs()
	active, ok := d.ActiveWorkspaceID()
	if !ok {
		return DashboardActionNone{}
	}
	index := slices.Index(ids, active)
	if index < 0 {
		return DashboardActionNone{}
	}
	next := index + delta
	if next < 0 || next >= len(ids) {
		return DashboardActionNone{}
	}
	return DashboardActionSelectWorkspace{WorkspaceID: ids[next]}
}

// SelectWorkspaceIndex selects the workspace in slot index, counting from
// zero, or does nothing when there is no tab there. This is the
// numbered-key sibling of [DashboardState.selectAdjacentWorkspace].
func (d *DashboardState) SelectWorkspaceIndex(index int) DashboardAction {
	ids := d.workspaceIDs()
	if index < 0 || index >= len(ids) {
		return DashboardActionNone{}
	}
	return DashboardActionSelectWorkspace{WorkspaceID: ids[index]}
}

// beginWorkspaceCommand opens the manager and requests its first snapshot
// off the UI loop.
func (d *DashboardState) beginWorkspaceCommand(close bool) DashboardAction {
	active := *d.activeWorkspaceID.Get()
	if active == nil {
		d.SetNotice("No active workspace")
		return DashboardActionNone{}
	}
	workspaceID := *active
	action := d.BeginWorkspaceManager()
	if manager := workspaceManagerRef(d.mode); manager != nil {
		manager.pendingCommand = &pendingWorkspaceCommand{workspaceID: workspaceID, close: close}
	}
	return action
}

// BeginWorkspaceManager opens the workspace manager and asks for its first
// snapshot.
func (d *DashboardState) BeginWorkspaceManager() DashboardAction {
	d.workspaceManagementGeneration++
	generation := d.workspaceManagementGeneration
	manager := workspaceManagerLoading(generation, workspacesCloneString(*d.activeWorkspaceID.Get()))
	manager.closingWorkspaces = maps.Clone(d.closingWorkspaces)
	if manager.closingWorkspaces == nil {
		manager.closingWorkspaces = map[string]struct{}{}
	}
	d.mode = ModeWorkspaceManager{&manager}
	return DashboardActionLoadWorkspaceManagement{Generation: generation}
}

// WorkspaceEntriesWithout is the visible manager's entries once the daemon
// has confirmed deleting workspaceID, so the dialog does not wait for a
// second query to learn what the confirmation already says. Empty when no
// manager is open.
func (d *DashboardState) WorkspaceEntriesWithout(workspaceID string) []WorkspaceManagementEntry {
	manager := workspaceManagerRef(d.mode)
	if manager == nil {
		return nil
	}
	var entries []WorkspaceManagementEntry
	for index := range manager.entries {
		if manager.entries[index].Workspace.ID != workspaceID {
			entries = append(entries, manager.entries[index].Clone())
		}
	}
	return entries
}

// WorkspaceCloseFinished clears operation state even when its original
// dialog has been dismissed. It returns the generation of a reopened
// progress dialog that owns this operation's result too.
func (d *DashboardState) WorkspaceCloseFinished(workspaceID string) (uint64, bool) {
	delete(d.closingWorkspaces, workspaceID)
	if manager := workspaceManagerInMode(&d.mode); manager != nil {
		delete(manager.closingWorkspaces, workspaceID)
		viewed, ok := manager.viewWorkspaceID()
		if manager.busy != nil && *manager.busy == WorkspaceMutationClose && ok && viewed == workspaceID {
			// A reopened progress dialog owns this operation's result too.
			return manager.generation, true
		}
	}
	return 0, false
}

// WorkspaceManagementIsCurrent reports whether a background workspace query
// or mutation may still update the visible manager. This has the same mode
// and generation guard as [DashboardState.FinishWorkspaceManagement].
func (d *DashboardState) WorkspaceManagementIsCurrent(generation uint64) bool {
	current, ok := workspaceManagerGeneration(d.mode)
	return generation == d.workspaceManagementGeneration && ok && current == generation
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
	if !d.WorkspaceManagementIsCurrent(generation) {
		return workspaceManagementOutcomeIgnored()
	}
	activeWorkspaceID := workspacesCloneString(*d.activeWorkspaceID.Get())
	_, foreground := d.mode.(ModeWorkspaceManager)
	manager := workspaceManagerInMode(&d.mode)
	if manager == nil {
		return workspaceManagementOutcomeIgnored()
	}
	// position is the index of the entry for workspaceID, if listed.
	position := func(workspaceID *string) (int, bool) {
		if workspaceID == nil {
			return 0, false
		}
		index := slices.IndexFunc(manager.entries, func(entry WorkspaceManagementEntry) bool {
			return entry.Workspace.ID == *workspaceID
		})
		return index, index >= 0
	}
	clampedSelection := func() int {
		return min(manager.selected, max(len(manager.entries)-1, 0))
	}
	if err != nil {
		// A details answer that failed after the list arrived keeps the list
		// read-only: its drafts are unknown, so no mutation may run on it.
		// Reopening the manager retries the load.
		detailsUnknown := manager.busy != nil && *manager.busy == WorkspaceMutationLoad && len(manager.entries) > 0
		manager.setError(err.Error())
		if detailsUnknown {
			busy := WorkspaceMutationLoad
			manager.busy = &busy
		}
		return WorkspaceManagementOutcome{Foreground: true, Action: DashboardActionNone{}}
	}
	closeWithoutConfirmation := false
	sortWorkspaceEntries(entries)
	if detailsPending {
		var previousSelectedID *string
		if entry := manager.selectedEntry(); entry != nil {
			id := entry.Workspace.ID
			previousSelectedID = &id
		}
		manager.entries = entries
		// The first answer has no previous selection: offer the active
		// workspace, as the complete answer does.
		wanted := previousSelectedID
		if wanted == nil {
			wanted = activeWorkspaceID
		}
		if index, ok := position(wanted); ok {
			manager.selected = index
		} else {
			manager.selected = clampedSelection()
		}
		manager.syncForm()
		return WorkspaceManagementOutcome{Foreground: foreground, Action: DashboardActionNone{}}
	}
	previousBusy := manager.busy
	// A rename or close started from its shortcut ends where the shortcut
	// was pressed, as leaving it with Esc does (B-11). Read before the new
	// entries can reset the view.
	returnToDashboard := manager.leavesToDashboard() && previousBusy != nil &&
		(*previousBusy == WorkspaceMutationRename || *previousBusy == WorkspaceMutationClose)
	var workspaceID *string
	if id, ok := manager.viewWorkspaceID(); ok {
		workspaceID = &id
	} else if entry := manager.selectedEntry(); entry != nil {
		id := entry.Workspace.ID
		workspaceID = &id
	} else {
		workspaceID = activeWorkspaceID
	}
	previousEntries := manager.entries
	manager.entries = entries
	selected, ok := position(workspaceID)
	if !ok {
		selected = clampedSelection()
	}
	var previousDraftID *string
	if manager.selected >= 0 && manager.selected < len(previousEntries) {
		drafts := previousEntries[manager.selected].Drafts
		if manager.selectedDraft >= 0 && manager.selectedDraft < len(drafts) {
			id := drafts[manager.selectedDraft].ID
			previousDraftID = &id
		}
	}
	manager.selected = selected
	manager.selectedDraft = 0
	if previousDraftID != nil {
		if entry := manager.selectedEntry(); entry != nil {
			if index := slices.IndexFunc(entry.Drafts, func(draft WorkspaceDraftEntry) bool {
				return draft.ID == *previousDraftID
			}); index >= 0 {
				manager.selectedDraft = index
			}
		}
	}
	if id, ok := manager.viewWorkspaceID(); ok {
		if _, listed := position(&id); !listed {
			manager.view = WorkspaceManagerViewList{}
		}
	}
	if previousBusy != nil && (*previousBusy == WorkspaceMutationCreate ||
		*previousBusy == WorkspaceMutationRename ||
		*previousBusy == WorkspaceMutationClose) {
		manager.view = WorkspaceManagerViewList{}
	}
	if previousBusy != nil && *previousBusy == WorkspaceMutationRecover {
		success := "Draft recovered."
		manager.success = &success
	} else {
		manager.success = nil
	}
	manager.loading = false
	manager.busy = nil
	manager.error = nil
	if _, list := manager.view.(WorkspaceManagerViewList); list {
		if entry := manager.selectedEntry(); entry != nil {
			manager.name = chat.TextInputFromValue(entry.Workspace.Name).WithMaxChars(64)
		} else {
			manager.name = chat.NewTextInput().WithMaxChars(64)
		}
	}
	if pending := manager.pendingCommand; pending != nil {
		manager.pendingCommand = nil
		if index, ok := position(&pending.workspaceID); ok {
			manager.selectIndex(index)
			closeWithoutConfirmation = pending.close && !closeNeedsConfirmation(&manager.entries[index])
			manager.openSelectedCommand(pending.close)
			manager.openedByShortcut = true
		} else {
			message := "The workspace no longer exists."
			manager.error = &message
		}
	}
	manager.syncForm()
	var action DashboardAction = DashboardActionNone{}
	if closeWithoutConfirmation {
		action = d.workspaceManagerMutation(WorkspaceMutationClose)
	}
	if _, open := d.mode.(ModeWorkspaceManager); returnToDashboard && open {
		d.CancelModal()
	}
	return WorkspaceManagementOutcome{Foreground: foreground, Action: action}
}

func (d *DashboardState) handleWorkspaceManagerEvent(event tea.Msg) DashboardAction {
	manager := workspaceManagerRef(d.mode)
	if manager == nil {
		return DashboardActionNone{}
	}
	result := manager.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	switch interaction := result.Action.(type) {
	case chat.InteractionCancel[WorkspaceControl]:
		if manager.busy != nil && *manager.busy == WorkspaceMutationClose {
			d.CancelModal()
			d.SetNotice("Deleting workspace in the background; reopen Delete workspace to cancel.")
			break
		}
		locked := manager.busy != nil && *manager.busy != WorkspaceMutationLoad
		if !locked {
			if _, list := manager.view.(WorkspaceManagerViewList); list || manager.leavesToDashboard() {
				d.CancelModal()
			} else {
				manager.resetToList()
			}
		}
	case chat.InteractionEdit[WorkspaceControl]:
		if interaction.ID == WorkspaceControlName &&
			chat.TextFieldApply(&manager.name, interaction.Edit) == chat.EditOutcomeChanged {
			manager.error = nil
			manager.success = nil
		}
	case chat.InteractionSelect[WorkspaceControl]:
		switch interaction.ID {
		case WorkspaceControlList:
			previous := manager.selected
			manager.selectIndex(interaction.Index)
			if manager.selected != previous {
				// An error from the details load belongs to the list, not the
				// selection; it stays until a reload retries it.
				if manager.busy == nil || *manager.busy != WorkspaceMutationLoad {
					manager.error = nil
				}
				manager.success = nil
			}
		case WorkspaceControlDraftList:
			if manager.selectedDraft != interaction.Index {
				manager.selectedDraft = interaction.Index
			}
		}
	case chat.InteractionActivate[WorkspaceControl]:
		switch control := interaction.ID; {
		case control == WorkspaceControlList || control == WorkspaceControlOpen:
			// Opening needs no details: it is allowed while the details load
			// runs or failed, not during a mutation.
			if manager.busy == nil || *manager.busy == WorkspaceMutationLoad {
				if entry := manager.selectedEntry(); entry != nil {
					workspaceID := entry.Workspace.ID
					d.CancelModal()
					return DashboardActionSelectWorkspace{WorkspaceID: workspaceID}
				}
			}
		case control == WorkspaceControlNew && manager.canMutate():
			manager.view = WorkspaceManagerViewCreate{}
			manager.name = chat.NewTextInput().WithMaxChars(64)
			manager.error = nil
			manager.success = nil
			manager.form.Focus(WorkspaceControlName)
		case control == WorkspaceControlRename && manager.canMutate():
			manager.openSelectedCommand(false)
		case control == WorkspaceControlContinueWorking:
			d.CancelModal()
			d.SetNotice("Deleting workspace in the background; reopen Delete workspace to cancel.")
		case control == WorkspaceControlCancelClose:
			if view, ok := manager.view.(WorkspaceManagerViewClose); ok {
				return DashboardActionCancelWorkspaceClose{WorkspaceID: view.WorkspaceID}
			}
		case control == WorkspaceControlConfirmClose:
			return d.workspaceManagerMutation(WorkspaceMutationClose)
		case control == WorkspaceControlDelete && manager.canMutate():
			selected := manager.selectedEntry()
			immediate := selected != nil && !closeNeedsConfirmation(selected)
			manager.openSelectedCommand(true)
			if immediate {
				return d.workspaceManagerMutation(WorkspaceMutationClose)
			}
		case control == WorkspaceControlDrafts && manager.canMutate():
			if entry := manager.selectedEntry(); entry != nil {
				manager.view = WorkspaceManagerViewDrafts{WorkspaceID: entry.Workspace.ID}
				manager.selectedDraft = 0
				manager.error = nil
				manager.success = nil
				manager.form.Focus(WorkspaceControlDraftList)
			}
		case control == WorkspaceControlBack:
			if _, list := manager.view.(WorkspaceManagerViewList); list || manager.leavesToDashboard() {
				d.CancelModal()
			} else {
				manager.resetToList()
			}
		case control == WorkspaceControlName:
			next := WorkspaceControlCancel
			switch manager.view.(type) {
			case WorkspaceManagerViewCreate:
				next = WorkspaceControlCreate
			case WorkspaceManagerViewRename:
				next = WorkspaceControlSave
			}
			manager.form.Focus(next)
		case control == WorkspaceControlCreate:
			return d.workspaceManagerMutation(WorkspaceMutationCreate)
		case control == WorkspaceControlSave:
			return d.workspaceManagerMutation(WorkspaceMutationRename)
		case control == WorkspaceControlDraftList || control == WorkspaceControlRecover:
			return d.workspaceManagerMutation(WorkspaceMutationRecover)
		case control == WorkspaceControlCancel:
			if _, closing := manager.view.(WorkspaceManagerViewClose); !manager.leavesToDashboard() && closing {
				manager.resetToList()
			} else {
				d.CancelModal()
			}
		}
	}
	return DashboardActionNone{}
}

func (d *DashboardState) workspaceManagerMutation(mutation WorkspaceMutation) DashboardAction {
	manager := workspaceManagerRef(d.mode)
	if manager == nil || !manager.canMutate() {
		return DashboardActionNone{}
	}
	generation := manager.generation
	var action DashboardAction
	switch mutation {
	case WorkspaceMutationCreate:
		name := strings.TrimSpace(manager.name.Value())
		if name == "" {
			message := "Workspace name cannot be empty."
			manager.error = &message
			return DashboardActionNone{}
		}
		action = DashboardActionCreateWorkspace{Generation: generation, Name: name}
	case WorkspaceMutationRename:
		view, ok := manager.view.(WorkspaceManagerViewRename)
		if !ok {
			return DashboardActionNone{}
		}
		name := strings.TrimSpace(manager.name.Value())
		if name == "" {
			message := "Workspace name cannot be empty."
			manager.error = &message
			return DashboardActionNone{}
		}
		action = DashboardActionRenameWorkspace{Generation: generation, WorkspaceID: view.WorkspaceID, Name: name}
	case WorkspaceMutationClose:
		view, ok := manager.view.(WorkspaceManagerViewClose)
		if !ok {
			return DashboardActionNone{}
		}
		if d.closingWorkspaces == nil {
			d.closingWorkspaces = map[string]struct{}{}
		}
		d.closingWorkspaces[view.WorkspaceID] = struct{}{}
		if manager.closingWorkspaces == nil {
			manager.closingWorkspaces = map[string]struct{}{}
		}
		manager.closingWorkspaces[view.WorkspaceID] = struct{}{}
		action = DashboardActionCloseWorkspace{Generation: generation, WorkspaceID: view.WorkspaceID}
	case WorkspaceMutationRecover:
		entry := manager.viewedEntry()
		if entry == nil || manager.selectedDraft < 0 || manager.selectedDraft >= len(entry.Drafts) {
			message := "This workspace has no detached drafts."
			manager.error = &message
			return DashboardActionNone{}
		}
		action = DashboardActionRecoverWorkspaceDraft{Generation: generation, DraftID: entry.Drafts[manager.selectedDraft].ID}
	default:
		return DashboardActionNone{}
	}
	busy := mutation
	manager.busy = &busy
	manager.error = nil
	manager.success = nil
	return action
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
	return WorkspaceManagementOutcome{Foreground: false, Action: DashboardActionNone{}}
}

// workspaceTabClick handles clicks on the workspace pane before row and
// pane hitboxes. It returns nil when the click is not the pane's.
func workspaceTabClick(dashboard *DashboardState, column, row int) DashboardAction {
	at := uv.Pos(column, row)
	if area := dashboard.workspaceDropdownArea; area != nil && at.In(*area) {
		dashboard.beginWorkspaceDropdown()
		dashboard.recordEventHandled()
		return DashboardActionNone{}
	}
	if area := dashboard.subagentWorkspaceCloseArea; area != nil && at.In(*area) {
		return DashboardActionExitSubagentWorkspace{}
	}
	var workspaceID *string
	for _, tab := range dashboard.workspaceTabAreas {
		if at.In(tab.Second) {
			id := tab.First
			workspaceID = &id
			break
		}
	}
	if area := dashboard.workspacePaneArea; area != nil && at.In(*area) && dashboard.focus != FocusWorkspaces {
		dashboard.focus = FocusWorkspaces
		dashboard.setSessionActionFocus(nil)
	}
	if workspaceID == nil {
		return nil
	}
	if dashboard.focus != FocusWorkspaces {
		dashboard.focus = FocusWorkspaces
	}
	if dashboard.workspaceControlFocus != WorkspaceControlFocusTabs {
		dashboard.workspaceControlFocus = WorkspaceControlFocusTabs
	}
	if active, ok := dashboard.ActiveWorkspaceID(); ok && active == *workspaceID {
		return nil
	}
	return DashboardActionSelectWorkspace{WorkspaceID: *workspaceID}
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
	ids := dashboard.workspaceIDs()
	labels := make([]string, len(ids))
	badges := make([]*chat.Span, len(ids))
	for index, id := range ids {
		labels[index] = dashboard.workspaceDisplayName(id)
		badges[index] = attentionBadge(dashboard.workspaceAttentionSummary(id))
	}
	var cells []workspaceTabCell
	row, x := 0, 0
	if width > 0 {
		for index, label := range labels {
			desired := chat.RawLine(" " + label + " ").Width()
			if badge := badges[index]; badge != nil {
				desired += badge.Width()
			}
			cellWidth := min(desired, width)
			if x > 0 && cellWidth > max(width-x, 0) {
				row++
				x = 0
			}
			cells = append(cells, workspaceTabCell{index: index, row: row, x: x, width: cellWidth})
			x += cellWidth
		}
	}
	rows := row + 1
	if len(cells) == 0 {
		rows = 1
	}
	return WorkspaceTabs{ids: ids, labels: labels, badges: badges, cells: cells, rows: rows}
}

func workspaceTabsFitting(dashboard *DashboardState, width int, visible int) WorkspaceTabs {
	tabs := newWorkspaceTabs(dashboard, width)
	if tabs.rows > visible {
		return newWorkspaceTabs(dashboard, max(width-1, 0))
	}
	return tabs
}

func workspacePaneHeight(dashboard *DashboardState, width int) int {
	if _, child := dashboard.SubagentParentID(); dashboard.PaneSize(SupportPaneSessions) == core.PaneSizeMinimized || child {
		return 3
	}
	tabs := workspaceTabsFitting(dashboard, max(width-(2+workspaceMenuWidth), 0), workspaceMaxRows)
	return min(tabs.rows, workspaceMaxRows) + 2
}

// renderWorkspaceTabs draws the workspace rows using the same cell layout
// used to measure them.
func renderWorkspaceTabs(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState) {
	dashboard.clearWorkspaceTabAreas()
	if area.Dy() <= 0 || area.Dx() <= 0 {
		dashboard.workspacePaneArea = nil
		dashboard.workspaceScrollbar.Clear()
		return
	}
	paneArea := area
	dashboard.workspacePaneArea = &paneArea
	focused := dashboard.Focus() == FocusWorkspaces
	title := chat.LineFrom(chat.StyledSpan(workspacesTitle, chat.Title(focused)))
	// The product mark and name sit beside the build number on the right;
	// the version keeps its muted, non-bold stamp style.
	stamp := chat.LineFrom(
		chat.StyledSpan(" "+chat.ThemeGlyphs().Spark+" MJOLNIR", chat.Title(true)),
		chat.StyledSpan(" "+dashboard.versionLabel+" ", chat.Muted().Remove(chat.ModifierBold)),
	).RightAligned()
	stampWidth := stamp.Width()
	titleWidth := title.Width()
	block := chat.Panel(focused).WithTitle(title)
	// The pane sits at the top of every dashboard, so its border is where
	// the product name and build number cost nothing and are always in
	// view. A sidebar too narrow to hold both drops them as a whole rather
	// than overlap the pane's own title.
	if area.Dx() >= titleWidth+stampWidth+borderCornerCells {
		block = block.WithTitle(stamp)
	}
	inner := block.Inner(area)
	frame.RenderWidget(block, area)
	if inner.Dx() <= 0 || inner.Dy() <= 0 {
		dashboard.workspaceScrollbar.Clear()
		return
	}
	if _, child := dashboard.SubagentParentID(); child {
		dashboard.workspaceScrollbar.Clear()
		chain := dashboard.subagentWorkspaceChain()
		segments := make([]string, 0, len(chain))
		for _, workspace := range chain {
			title := workspace.ParentID
			if session := dashboard.state.Get().Sessions.GetShared(workspace.ParentID); session != nil {
				title = session.ListedTitle()
			}
			title = strings.Join(strings.Fields(title), " ")
			if workspace.Kind == core.SubagentKindReviewer {
				title = "Reviewers · " + title
			}
			segments = append(segments, title)
		}
		closeWidth := min(3, inner.Dx())
		closeArea := uv.Rect(max(inner.Max.X-closeWidth, 0), inner.Min.Y, closeWidth, 1)
		dashboard.subagentWorkspaceCloseArea = &closeArea
		labelArea := uv.Rect(inner.Min.X, inner.Min.Y, max(inner.Dx()-closeWidth, 0), 1)
		frame.RenderWidget(
			chat.NewParagraph(chat.RawText(workspaceBreadcrumb(
				segments,
				labelArea.Dx(),
				dashboard.ReviewersWorkspaceOpen(),
			))).WithStyle(chat.Selection(true)),
			labelArea,
		)
		closeLabel := "X"
		if closeWidth == 3 {
			closeLabel = "[X]"
		}
		frame.RenderWidget(chat.NewParagraph(chat.RawText(closeLabel)).WithStyle(chat.ActionableChip()), closeArea)
		return
	}
	menuWidth := min(3, inner.Dx())
	menuArea := uv.Rect(max(inner.Max.X-menuWidth, 0), inner.Min.Y, menuWidth, 1)
	if menuWidth == 3 {
		dashboard.workspaceHamburgerArea = &menuArea
		renderWorkspaceMenu(frame, menuArea, dashboard)
	}
	tabsWidth := max(inner.Dx()-menuWidth, 0)
	active, hasActive := dashboard.ActiveWorkspaceID()
	if dashboard.PaneSize(SupportPaneSessions) == core.PaneSizeMinimized {
		ids := dashboard.workspaceIDs()
		index := -1
		if hasActive {
			index = slices.Index(ids, active)
		}
		if index < 0 && len(ids) > 0 {
			index = 0
		}
		if index >= 0 {
			id := ids[index]
			tabsArea := uv.Rect(inner.Min.X, inner.Min.Y, tabsWidth, 1)
			dashboard.workspaceDropdownArea = &tabsArea
			labelWidth := max(tabsWidth-2, 0)
			line := workspaceTabLine(
				dashboard.workspaceDisplayName(id),
				attentionBadge(dashboard.workspaceAttentionSummary(id)),
				labelWidth,
				chat.ActiveControl(),
			)
			line.Spans = append(line.Spans, chat.StyledSpan(chat.ThemeGlyphs().Dropdown+" ", chat.Actionable()))
			frame.RenderWidget(chat.NewParagraph(chat.TextFrom(line)), tabsArea)
		}
		dashboard.workspaceScrollbar.Clear()
		return
	}
	visible := inner.Dy()
	tabs := workspaceTabsFitting(dashboard, tabsWidth, visible)
	selected := 0
	if hasActive {
		if index := slices.Index(tabs.ids, active); index >= 0 {
			selected = index
		}
	}
	selectedRow := 0
	for _, cell := range tabs.cells {
		if cell.index == selected {
			selectedRow = cell.row
			break
		}
	}
	var selectionLayout *workspaceScrollSelection
	if selected < len(tabs.ids) {
		selectionLayout = &workspaceScrollSelection{
			workspaceID: tabs.ids[selected],
			row:         selectedRow,
			tabsWidth:   tabsWidth,
			height:      inner.Dy(),
		}
	}
	first := min(dashboard.workspaceScroll, max(tabs.rows-visible, 0))
	// Scrolling may intentionally hide the active workspace. Reveal it only
	// when selection or layout changes, not on every redraw after the wheel.
	previous := dashboard.workspaceScrollSelected
	if (previous == nil) != (selectionLayout == nil) || (previous != nil && *previous != *selectionLayout) {
		if selectedRow < first {
			first = selectedRow
		}
		if selectedRow >= first+visible {
			first = selectedRow + 1 - visible
		}
	}
	dashboard.workspaceScrollSelected = selectionLayout
	dashboard.workspaceScroll = first
	dashboard.workspaceScrollGeometry = &client.DaemonPair[int, int]{First: tabs.rows, Second: visible}
	for _, cell := range tabs.cells {
		if cell.row < first || cell.row >= first+visible {
			continue
		}
		tabArea := uv.Rect(inner.Min.X+cell.x, inner.Min.Y+(cell.row-first), cell.width, 1)
		dashboard.registerWorkspaceTabArea(tabs.ids[cell.index], tabArea)
		style := chat.Actionable()
		if cell.index == selected {
			style = chat.ActiveControl()
		}
		line := workspaceTabLine(tabs.labels[cell.index], tabs.badges[cell.index], cell.width, style)
		frame.RenderWidget(chat.NewParagraph(chat.TextFrom(line)), tabArea)
	}
	var geometry chat.ScrollbarGeometry
	hasGeometry := false
	if tabs.rows > visible {
		geometry, hasGeometry = chat.NewScrollbarGeometry(
			uv.Rect(inner.Min.X+max(tabsWidth-1, 0), inner.Min.Y, 1, inner.Dy()),
			tabs.rows,
			first,
			visible,
		)
	}
	dashboard.workspaceScrollbar.SetGeometry(geometry, hasGeometry)
	if hasGeometry {
		chat.RenderScrollbar(frame, geometry)
	}
}

// workspaceTabLine is one tab: the name, then the attention badge when there
// is one and the tab has room for the name (or at least three cells of a
// longer name). The badge keeps its own colour on an unselected tab; a
// selected tab is already the highlight colour, and the counts stay legible
// in it.
func workspaceTabLine(name string, badge *chat.Span, width int, style chat.Style) chat.Line {
	minimumNameWidth := min(chat.RawLine(name).Width(), 3) + 2
	if badge == nil || width < badge.Width()+minimumNameWidth {
		return chat.StyledLine(workspaceLabel(name, width), style)
	}
	nameWidth := max(width-badge.Width(), 0)
	content := truncateToCells(name, max(nameWidth-2, 0), TruncatePlain)
	pad := strings.Repeat(" ", max(nameWidth-(chat.RawLine(content).Width()+2), 0))
	background := style.Bg
	if !background.IsSet() {
		// Rust's `Color::default()`.
		background = chat.ColorReset
	}
	return chat.LineFrom(
		chat.StyledSpan(" "+content, style),
		chat.StyledSpan(badge.Content, badge.Style.Background(background)),
		chat.StyledSpan(pad+" ", style),
	)
}

func workspaceBreadcrumb(segments []string, width int, reviewers bool) string {
	const separator = " › "
	const elided = "… › "
	if len(segments) == 0 {
		return ""
	}
	deepest := segments[len(segments)-1]
	padding := ""
	if width >= 3 {
		padding = " "
	}
	available := width - 2*len(padding)
	deepestTitle := deepest
	if reviewers {
		deepestTitle = strings.TrimPrefix(deepest, "Reviewers · ")
	}
	for index := range segments {
		prefix := ""
		if index > 0 {
			prefix = elided
		}
		path := prefix + strings.Join(segments[index:], separator)
		if chat.RawLine(path).Width() <= available {
			return padding + path + padding
		}
	}
	title := deepestTitle
	if chat.RawLine(deepest).Width() <= available {
		title = deepest
	}
	title = truncateToCells(title, available, TruncatePlain)
	return padding + title + padding
}

func workspaceLabel(name string, width int) string {
	if width <= 2 {
		return strings.Repeat(" ", max(width, 0))
	}
	content := truncateToCells(name, width-2, TruncatePlain)
	return " " + content + " "
}

// renderWorkspaceManager is the standard Dialog modal renderer used by the
// parent combined renderer.
func renderWorkspaceManager(frame *chat.Frame, area uv.Rectangle, dialog *WorkspaceManager, surfaces *chat.FrameSurfaces) {
	popup := centeredModal(frame, surfaces, 72, min(area.Dy(), 24), area)
	var title string
	switch dialog.view.(type) {
	case WorkspaceManagerViewList:
		title = " Workspaces "
	case WorkspaceManagerViewCreate:
		title = " Workspaces · New "
	case WorkspaceManagerViewRename:
		title = " Workspaces · Rename "
	case WorkspaceManagerViewClose:
		title = " Workspaces · Delete "
	default:
		title = " Workspaces · Drafts "
	}
	inner := chat.RectInner(popup, chat.Margin{Horizontal: 1, Vertical: 1})
	if inner.Dy() < 5 {
		dialog.form.ResetGeometry()
		return
	}
	rows := chat.VerticalLayout(
		chat.ConstraintLength(1),
		chat.ConstraintMin(3),
		chat.ConstraintLength(1),
	).Split(inner)
	busyIs := func(mutation WorkspaceMutation) bool {
		return dialog.busy != nil && *dialog.busy == mutation
	}
	var message string
	switch {
	case dialog.loading && len(dialog.entries) == 0:
		message = "Loading workspaces" + chat.ThemeGlyphs().Ellipsis
	case dialog.loading:
		message = "Loading details" + chat.ThemeGlyphs().Ellipsis
	case dialog.error != nil:
		message = *dialog.error
	case dialog.success != nil:
		message = *dialog.success
	default:
		switch dialog.view.(type) {
		case WorkspaceManagerViewList:
			message = "Enter opens the selected workspace · Tab moves between controls"
		// Esc from a page leaves it for the list, unless the page was opened
		// by its shortcut, which never showed the list (B-11).
		case WorkspaceManagerViewCreate:
			message = "Enter creates the workspace · Esc returns to the list"
		case WorkspaceManagerViewRename:
			if dialog.leavesToDashboard() {
				message = "Enter saves the new name · Esc cancels"
			} else {
				message = "Enter saves the new name · Esc returns to the list"
			}
		case WorkspaceManagerViewClose:
			if busyIs(WorkspaceMutationClose) {
				message = "Suspending sessions · Esc returns to the dashboard"
			} else {
				message = "Confirm deleting the workspace · Esc cancels"
			}
		default:
			message = "Enter recovers the selected draft · Esc returns to the list"
		}
	}
	messageColor := chat.ThemePalette().Muted
	if dialog.error != nil {
		messageColor = chat.ThemePalette().Error
	} else if dialog.success != nil {
		messageColor = chat.ThemePalette().Success
	}
	frame.RenderWidget(chat.NewParagraph(chat.RawText(message)).WithStyle(chat.Style{}.Foreground(messageColor)), rows[0])
	form := &dialog.form
	form.BeginFrame()
	// The actions form a column beside the page body rather than a footer
	// row, so the body gives up exactly the width that column needs.
	columns := form.SplitActions(rows[1], dialog.actions())
	dismissEnabled := dialog.busy == nil || busyIs(WorkspaceMutationLoad) || busyIs(WorkspaceMutationClose)
	titleLine := dismissibleModalTitle(&form.Form, popup, strings.TrimSpace(title), chat.Title(true), dismissEnabled)
	frame.RenderWidget(chat.Modal().WithTitle(titleLine), popup)
	switch view := dialog.view.(type) {
	case WorkspaceManagerViewList:
		renderManagerList(frame, columns, dialog, form)
	case WorkspaceManagerViewCreate:
		renderManagerNameView(frame, columns, dialog, form, "New workspace name:", WorkspaceControlCreate, "Create")
	case WorkspaceManagerViewRename:
		renderManagerNameView(frame, columns, dialog, form, "Workspace name:", WorkspaceControlSave, "Save")
	case WorkspaceManagerViewClose:
		sessions := counted(view.SessionCount, "session", "sessions")
		drafts := counted(view.DraftCount, "saved draft", "saved drafts")
		text := fmt.Sprintf(
			"Delete %s?\n\nSuspend %s and stop their sub-agents.\nResumable histories are preserved.\nDiscard %s and any unsent composer text.\nRemove this workspace after all sessions are suspended.",
			core.RustDebugString(view.WorkspaceName), sessions, drafts,
		)
		frame.RenderWidget(chat.NewParagraph(chat.RawText(text)).WithWrap(chat.Wrap{Trim: false}), columns.Body)
		form.RenderActionsStacked(frame, columns.Actions, dialog.actions(), chat.ColumnAlignRight)
	default:
		renderManagerDrafts(frame, columns, dialog, form)
	}
	busy := ""
	if dialog.busy != nil {
		if *dialog.busy == WorkspaceMutationClose {
			busy = "Working: deleting workspace"
		} else {
			busy = "Working: " + string(*dialog.busy)
		}
	}
	frame.RenderWidget(chat.NewParagraph(chat.RawText(busy)).WithStyle(chat.Style{}.Foreground(chat.ThemePalette().Muted)), rows[2])
	var initial WorkspaceControl
	switch dialog.view.(type) {
	case WorkspaceManagerViewList:
		if len(dialog.entries) == 0 {
			initial = WorkspaceControlNew
		} else {
			initial = WorkspaceControlList
		}
	case WorkspaceManagerViewCreate, WorkspaceManagerViewRename:
		initial = WorkspaceControlName
	case WorkspaceManagerViewClose:
		initial = WorkspaceControlCancel
	default:
		initial = WorkspaceControlDraftList
	}
	form.EndFrame(initial)
}

func renderManagerList(frame *chat.Frame, columns chat.ColumnSplit, dialog *WorkspaceManager, form *chat.Dialog[WorkspaceControl]) {
	body, actions := columns.Body, columns.Actions
	items := make([]chat.Line, 0, len(dialog.entries))
	for _, entry := range dialog.entries {
		current := ""
		if dialog.activeWorkspaceID != nil && *dialog.activeWorkspaceID == entry.Workspace.ID {
			current = "  Current"
		}
		sessions := counted(entry.Workspace.SessionCount, "session", "sessions")
		drafts := ""
		if len(entry.Drafts) > 0 {
			drafts = "  " + counted(len(entry.Drafts), "draft", "drafts")
		}
		items = append(items, chat.RawLine(entry.Workspace.Name+current+"  "+sessions+drafts))
	}
	if len(items) == 0 {
		frame.RenderWidget(chat.NewParagraph(chat.RawText("No workspaces.")), body)
		form.Register(WorkspaceControlList, chat.ControlKindChoiceList{Len: 0, Selected: 0}, body, false)
	} else {
		chat.ChoiceListRender(frame, body, items, dialog.selected, &form.Form, WorkspaceControlList)
	}
	form.RenderActionsStacked(frame, actions, dialog.actions(), chat.ColumnAlignRight)
}

func renderManagerNameView(frame *chat.Frame, columns chat.ColumnSplit, dialog *WorkspaceManager, form *chat.Dialog[WorkspaceControl], label string, submit WorkspaceControl, submitLabel string) {
	body, actions := columns.Body, columns.Actions
	field := uv.Rect(body.Min.X, body.Min.Y, body.Dx(), 1)
	labelWidth := chat.RawLine(label).Width() + 1
	frame.RenderWidget(
		chat.NewParagraph(chat.TextFrom(chat.LineFrom(chat.StyledSpan(label, chat.Style{}.Bold())))),
		uv.Rect(field.Min.X, field.Min.Y, min(labelWidth, field.Dx()), 1),
	)
	chat.TextFieldRender(
		frame,
		uv.Rect(field.Min.X+labelWidth, field.Min.Y, max(field.Dx()-labelWidth, 0), 1),
		&dialog.name,
		&form.Form,
		WorkspaceControlName,
	)
	form.RenderActionsStacked(frame, actions, dialog.actions(), chat.ColumnAlignRight)
}

func renderManagerDrafts(frame *chat.Frame, columns chat.ColumnSplit, dialog *WorkspaceManager, form *chat.Dialog[WorkspaceControl]) {
	body, actions := columns.Body, columns.Actions
	var drafts []chat.Line
	if entry := dialog.viewedEntry(); entry != nil {
		for _, draft := range entry.Drafts {
			session := ""
			if draft.SessionID != nil {
				session = "  · " + *draft.SessionID
			}
			drafts = append(drafts, chat.RawLine(draft.Source+"  · "+draft.SavedAt+session))
		}
	}
	if len(drafts) == 0 {
		frame.RenderWidget(chat.NewParagraph(chat.RawText("No detached drafts.")), body)
		form.Register(WorkspaceControlDraftList, chat.ControlKindChoiceList{Len: 0, Selected: 0}, body, false)
	} else {
		chat.ChoiceListRender(frame, body, drafts, dialog.selectedDraft, &form.Form, WorkspaceControlDraftList)
	}
	form.RenderActionsStacked(frame, actions, dialog.actions(), chat.ColumnAlignRight)
}

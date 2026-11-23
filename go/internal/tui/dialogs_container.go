// Port of mj-tui/src/dialogs/container.rs.
//
// Container settings composed from reusable terminal controls.

package tui

import (
	"fmt"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ContainerEditor edits one session's container size and attached
// directories.
type ContainerEditor struct {
	sessionID string
	// sessionName is what the header calls the session: its title, as the
	// row and the other session dialogs do, not its id.
	sessionName string
	cpus        chat.TextInput
	// limits is the host's CPUs and memory bytes; nil when unknown.
	limits *HostLimits
	// originalAllocation is the allocation the session had when the editor
	// opened; nil for none.
	originalAllocation core.SessionResourceAllocation
	memory             chat.TextInput
	mounts             []core.AdditionalMount
	suggestions        []string
	source             chat.PathInput
	destination        chat.PathInput
	access             core.MountAccess
	accessCombo        chat.ComboBoxState[ContainerEditFocus]
	form               chat.Dialog[ContainerEditFocus]
	mountIndex         int
	suggestionIndex    int
	error              *string
	// editingMount is the listed attachment the editor fields are editing,
	// when they were loaded from one. Adding then replaces that entry
	// instead of appending.
	editingMount *int
	// buildCache is the build cache this session was provisioned with, if it
	// got one. Read only: it is fixed when the container's mounts are, so
	// changing it here could not affect the running session.
	buildCache *string
}

// Clone is Rust's derived Clone: the form is copied, not shared.
func (e *ContainerEditor) Clone() ContainerEditor {
	clone := *e
	clone.mounts = append([]core.AdditionalMount(nil), e.mounts...)
	clone.suggestions = append([]string(nil), e.suggestions...)
	clone.form = e.form.Clone()
	return clone
}

// ContainerEditFocus is a control of the container editor.
type ContainerEditFocus int

const (
	ContainerEditFocusCpus ContainerEditFocus = iota
	ContainerEditFocusMemory
	ContainerEditFocusSource
	ContainerEditFocusDestination
	ContainerEditFocusAccess
	ContainerEditFocusMounts
	ContainerEditFocusSuggestions
	ContainerEditFocusCancel
	ContainerEditFocusSave
)

const containerEditScope = "Replace the container where supported, or save for its next creation."

// containerRow is one drawn row of the container editor (Rust's private
// `Row`, renamed for setup/search.rs's `Row` in the same package).
type containerRow interface {
	isContainerRow()
}

type containerRowText struct {
	line chat.Line
}

type containerRowField struct {
	id    ContainerEditFocus
	label string
	input *chat.TextInput
}

type containerRowAccess struct {
	kind chat.ControlKind
}

type containerRowList struct {
	id       ContainerEditFocus
	lines    []chat.Line
	selected int
}

func (containerRowText) isContainerRow()   {}
func (containerRowField) isContainerRow()  {}
func (containerRowAccess) isContainerRow() {}
func (containerRowList) isContainerRow()   {}

// containerRowHeight is Row::height.
func containerRowHeight(row containerRow) int {
	if list, ok := row.(containerRowList); ok {
		return max(len(list.lines), 1)
	}
	return 1
}

// containerRowControl is Row::control: the control the row registers, false
// for a text row.
func containerRowControl(row containerRow, editor *ContainerEditor) (ContainerEditFocus, chat.ControlKind, bool) {
	switch row := row.(type) {
	case containerRowField:
		if row.id == ContainerEditFocusSource {
			return ContainerEditFocusSource, editor.source.ControlKind(), true
		}
		return row.id, chat.ControlKindTextField{}, true
	case containerRowAccess:
		return ContainerEditFocusAccess, row.kind, true
	case containerRowList:
		return row.id, chat.ControlKindChoiceList{Len: len(row.lines), Selected: row.selected}, true
	default:
		return 0, nil, false
	}
}

func (e *ContainerEditor) prepareDialogState() {
	values := []string{
		e.cpus.String(),
		e.memory.String(),
		e.source.String(),
		e.destination.String(),
		fmt.Sprintf("%#v", e.mounts),
		fmt.Sprintf("%#v", e.access),
	}
	form := &e.form
	form.TrackDraft(values)
	form.SetDismissActions([]ContainerEditFocus{ContainerEditFocusCancel})
	form.SetDefaultAction(ContainerEditFocusSave)
	form.SetSubmit(ContainerEditFocusCpus, ContainerEditFocusSave)
	form.SetSubmit(ContainerEditFocusMemory, ContainerEditFocusSave)
}

func (e *ContainerEditor) focused() ContainerEditFocus {
	if id, ok := e.form.Focused(); ok {
		return id
	}
	return ContainerEditFocusCpus
}

// field is the focused text field; nil when the focus is not on one.
func (e *ContainerEditor) field() *chat.TextInput {
	switch e.focused() {
	case ContainerEditFocusCpus:
		return &e.cpus
	case ContainerEditFocusMemory:
		return &e.memory
	case ContainerEditFocusSource:
		return &e.source.TextInput
	case ContainerEditFocusDestination:
		return &e.destination.TextInput
	default:
		return nil
	}
}

func (e *ContainerEditor) fieldMut(id ContainerEditFocus) *chat.TextInput {
	switch id {
	case ContainerEditFocusCpus:
		return &e.cpus
	case ContainerEditFocusMemory:
		return &e.memory
	case ContainerEditFocusSource:
		return &e.source.TextInput
	case ContainerEditFocusDestination:
		return &e.destination.TextInput
	default:
		return nil
	}
}

func (e *ContainerEditor) rows() []containerRow {
	panic("unported: mj-tui/src/dialogs/container.rs ContainerEditor::rows")
}

// prepare refreshes eligibility after a domain change, including before the
// first frame.
func (e *ContainerEditor) prepare() {
	form := &e.form
	form.BeginFrame()
	for _, row := range e.rows() {
		if id, kind, ok := containerRowControl(row, e); ok {
			form.Register(id, kind, uv.Rectangle{}, true)
		}
	}
	form.Register(ContainerEditFocusCancel, chat.ControlKindButton{}, uv.Rectangle{}, true)
	form.Register(ContainerEditFocusSave, chat.ControlKindButton{}, uv.Rectangle{}, true)
	form.EndFrame(ContainerEditFocusCpus)
}

// addMount adds the typed mount, filling in a default destination, or
// replaces the listed one the fields were loaded from. It returns the reason
// it was rejected, if it was.
func (e *ContainerEditor) addMount() *string {
	panic("unported: mj-tui/src/dialogs/container.rs ContainerEditor::add_mount")
}

// editSelectedMount loads the selected attached directory into the editor
// fields, so its source, destination, and access mode are all edited the
// same way a new attachment is entered.
func (e *ContainerEditor) editSelectedMount() {
	if e.mountIndex < 0 || e.mountIndex >= len(e.mounts) {
		return
	}
	mount := e.mounts[e.mountIndex]
	e.source = chat.PathInputFromValue(mount.Source)
	e.destination = chat.PathInputFromValue(mount.Destination)
	e.access = mount.Access
	index := e.mountIndex
	e.editingMount = &index
	e.error = nil
	e.form.Focus(ContainerEditFocusSource)
}

func (e *ContainerEditor) takeSuggestion() {
	if e.suggestionIndex < 0 || e.suggestionIndex >= len(e.suggestions) {
		return
	}
	source := e.suggestions[e.suggestionIndex]
	e.source = chat.PathInputFromValue(source)
	e.destination = chat.PathInputFromValue(core.DefaultMountDestination(source, e.mounts))
	e.form.Focus(ContainerEditFocusSource)
}

func (e *ContainerEditor) removeSelected() {
	panic("unported: mj-tui/src/dialogs/container.rs ContainerEditor::remove_selected")
}

func (e *ContainerEditor) save() (DashboardAction, error) {
	panic("unported: mj-tui/src/dialogs/container.rs ContainerEditor::save")
}

func renderContainerEditor(frame *chat.Frame, area uv.Rectangle, editor *ContainerEditor, surfaces *chat.FrameSurfaces) {
	panic("unported: mj-tui/src/dialogs/container.rs render_container_editor")
}

func (d *DashboardState) beginContainerEdit() {
	panic("unported: mj-tui/src/dialogs/container.rs DashboardState::begin_container_edit")
}

// ContainerPathResolved takes the host's resolution of the attached
// directory typed for context: the resolved path, or the error that stopped
// it.
func (d *DashboardState) ContainerPathResolved(context string, path string, err error) {
	panic("unported: mj-tui/src/dialogs/container.rs DashboardState::container_path_resolved")
}

func (d *DashboardState) handleContainerEditEvent(event tea.Msg, editor ContainerEditor) DashboardAction {
	panic("unported: mj-tui/src/dialogs/container.rs DashboardState::handle_container_edit_event")
}

// focusedPathInput is CompletesPaths::focused_path_input. The attached
// directory lives on the session's own target host; the container
// destination does not exist yet, so it does not complete.
func (e *ContainerEditor) focusedPathInput(dashboard *DashboardState) (*chat.PathInput, core.CompletionHost, core.CompletionKind, bool) {
	panic("unported: mj-tui/src/dialogs/container.rs ContainerEditor::focused_path_input")
}

// dismissUnfocusedCompletions is CompletesPaths::dismiss_unfocused_completions.
func (e *ContainerEditor) dismissUnfocusedCompletions() {
	if e.focused() != ContainerEditFocusSource {
		e.source.DismissCompletion()
	}
}

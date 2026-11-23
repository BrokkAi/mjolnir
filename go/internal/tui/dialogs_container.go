// Port of mj-tui/src/dialogs/container.rs.
//
// Container settings composed from reusable terminal controls.

package tui

import (
	"fmt"
	"reflect"
	"slices"
	"strconv"
	"strings"

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
	muted := chat.Style{}.Foreground(chat.ThemePalette().Muted)
	rows := []containerRow{
		containerRowText{chat.RawLine("Session: " + FitSessionName(e.sessionName, 58))},
		containerRowText{chat.StyledLine(containerEditScope, muted)},
		containerRowText{chat.RawLine("")},
		containerRowField{ContainerEditFocusCpus, "CPUs", &e.cpus},
		containerRowField{ContainerEditFocusMemory, "Memory (GiB)", &e.memory},
		containerRowText{chat.StyledLine("Empty keeps the target's value.", muted)},
		containerRowText{chat.RawLine("")},
		containerRowText{chat.RawLine("Attached directories")},
	}
	if len(e.mounts) == 0 {
		rows = append(rows, containerRowText{chat.StyledLine("  none", muted)})
	} else {
		lines := make([]chat.Line, 0, len(e.mounts))
		for _, mount := range e.mounts {
			lines = append(lines, chat.RawLine(fmt.Sprintf("%s -> %s%s", mount.Source, mount.Destination, accessMarker(mount.Access))))
		}
		rows = append(rows, containerRowList{ContainerEditFocusMounts, lines, e.mountIndex})
	}
	rows = append(rows,
		containerRowText{chat.RawLine("")},
		containerRowField{ContainerEditFocusSource, "Attach host directory", &e.source.TextInput},
		containerRowField{ContainerEditFocusDestination, "Container destination", &e.destination.TextInput},
		containerRowAccess{accessComboKind(&e.accessCombo, accessChoices(false), e.access, ContainerEditFocusAccess)},
	)
	if len(e.suggestions) > 0 {
		lines := make([]chat.Line, 0, len(e.suggestions))
		for _, path := range e.suggestions {
			lines = append(lines, chat.RawLine(path))
		}
		rows = append(rows,
			containerRowText{chat.RawLine("")},
			containerRowText{chat.RawLine("Remembered directories")},
			containerRowList{ContainerEditFocusSuggestions, lines, e.suggestionIndex},
		)
	}
	// No reason is recorded, only the outcome. The machine's settings page
	// explains what a host does or does not offer.
	buildCache := "  none for this session"
	if e.buildCache != nil {
		buildCache = "  " + *e.buildCache
	}
	rows = append(rows,
		containerRowText{chat.RawLine("")},
		containerRowText{chat.RawLine("Build cache")},
		containerRowText{chat.StyledLine(buildCache, muted)},
	)
	if e.error != nil {
		rows = append(rows, containerRowText{chat.StyledLine(*e.error, chat.Style{}.Foreground(chat.ThemePalette().Warning))})
	}
	return rows
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
	source := strings.TrimSpace(e.source.Value())
	if source == "" {
		message := "Enter a host directory to attach."
		return &message
	}
	destination := strings.TrimSpace(e.destination.Value())
	if destination == "" {
		destination = core.DefaultMountDestination(source, e.mounts)
	}
	mount := core.AdditionalMount{Source: source, Destination: destination, Access: e.access}
	mounts := slices.Clone(e.mounts)
	var index int
	if e.editingMount != nil && *e.editingMount < len(mounts) {
		index = *e.editingMount
		mounts[index] = mount
	} else {
		mounts = append(mounts, mount)
		index = len(mounts) - 1
	}
	if err := core.ValidateAdditionalMounts(mounts); err != nil {
		message := err.Error()
		return &message
	}
	e.mounts = mounts
	e.source.Clear()
	e.destination.Clear()
	e.access = core.MountAccessRo
	e.editingMount = nil
	e.mountIndex = index
	return nil
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
	switch e.focused() {
	case ContainerEditFocusMounts:
		if len(e.mounts) == 0 {
			return
		}
		e.mounts = slices.Delete(e.mounts, e.mountIndex, e.mountIndex+1)
		// The remembered index no longer names the same entry.
		e.editingMount = nil
		e.mountIndex = min(e.mountIndex, max(len(e.mounts)-1, 0))
		if len(e.mounts) == 0 {
			e.form.Focus(ContainerEditFocusSource)
		}
	case ContainerEditFocusSuggestions:
		if len(e.suggestions) == 0 {
			return
		}
		e.suggestions = slices.Delete(e.suggestions, e.suggestionIndex, e.suggestionIndex+1)
		e.suggestionIndex = min(e.suggestionIndex, max(len(e.suggestions)-1, 0))
		if len(e.suggestions) == 0 {
			e.form.Focus(ContainerEditFocusSource)
		}
	}
}

func (e *ContainerEditor) save() (DashboardAction, error) {
	if err := core.ValidateAdditionalMounts(e.mounts); err != nil {
		return nil, err
	}
	draft := ResourceEditor{cpu: e.cpus, memory: e.memory}
	var allocation core.SessionResourceAllocation
	if strings.TrimSpace(e.cpus.Value()) != "" || strings.TrimSpace(e.memory.Value()) != "" {
		sized, err := draft.allocation(e.limits, false)
		if err != nil {
			return nil, err
		}
		allocation = sized
	}
	if allocation != nil && !reflect.DeepEqual(allocation, e.originalAllocation) {
		if err := allocation.ValidateMinimumUsefulContainer(); err != nil {
			return nil, err
		}
	}
	return DashboardActionSaveContainerSettings{
		SessionID:          e.sessionID,
		ResourceAllocation: allocation,
		AdditionalMounts:   slices.Clone(e.mounts),
		MountHistory:       slices.Clone(e.suggestions),
	}, nil
}

func renderContainerEditor(frame *chat.Frame, area uv.Rectangle, editor *ContainerEditor, surfaces *chat.FrameSurfaces) {
	rows := editor.rows()
	total := 0
	for _, row := range rows {
		total += containerRowHeight(row)
	}
	popup := centeredModal(frame, surfaces, 70, max(total+5, 18), area)
	inner := chat.RectInner(popup, chat.Margin{Horizontal: 1, Vertical: 1})
	body := uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), max(inner.Dy()-2, 0))
	focused := editor.focused()
	var focusRow *chat.RowRange
	rowY := 0
	for _, row := range rows {
		if id, _, ok := containerRowControl(row, editor); ok && id == focused {
			offset := 0
			if list, isList := row.(containerRowList); isList {
				offset = list.selected
			}
			focusRow = &chat.RowRange{Start: rowY + offset, End: rowY + offset + 1}
		}
		rowY += containerRowHeight(row)
	}
	form := &editor.form
	form.BeginFrame()
	viewport := form.Viewport(body, total, focusRow)
	title := dismissibleModalTitle(&form.Form, popup, "Edit container size and mounts", chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	rowY = 0
	choices := accessChoices(false)
	var accessField, sourceField *uv.Rectangle
	for _, row := range rows {
		height := containerRowHeight(row)
		rect := viewport.Row(rowY, height)
		visibleHeight := rect.Dy()
		switch row := row.(type) {
		case containerRowText:
			if visibleHeight > 0 {
				frame.RenderWidget(row.line, rect)
			}
		case containerRowField:
			label := row.label + ": "
			labelWidth := min(chat.RawLine(label).Width(), max(rect.Dx()-1, 0))
			if visibleHeight > 0 {
				frame.RenderWidget(chat.RawLine(label), uv.Rect(rect.Min.X, rect.Min.Y, labelWidth, rect.Dy()))
			}
			fieldArea := uv.Rect(rect.Min.X+labelWidth, rect.Min.Y, max(rect.Dx()-labelWidth, 0), rect.Dy())
			switch row.id {
			case ContainerEditFocusSource:
				sourceField = &fieldArea
				chat.PathFieldRender(frame, fieldArea, &editor.source, &form.Form, row.id)
			case ContainerEditFocusDestination:
				chat.PathFieldRender(frame, fieldArea, &editor.destination, &form.Form, row.id)
			default:
				chat.TextFieldRender(frame, fieldArea, row.input, &form.Form, row.id)
			}
		case containerRowAccess:
			label := "Access: "
			labelWidth := min(chat.RawLine(label).Width(), max(rect.Dx()-1, 0))
			if visibleHeight > 0 {
				frame.RenderWidget(chat.RawLine(label), uv.Rect(rect.Min.X, rect.Min.Y, labelWidth, rect.Dy()))
			}
			field := uv.Rect(rect.Min.X+labelWidth, rect.Min.Y, max(rect.Dx()-labelWidth, 0), rect.Dy())
			renderAccessCombo(frame, inner, field, editor.access, choices, &editor.accessCombo, false, &form.Form, ContainerEditFocusAccess)
			accessField = &field
		case containerRowList:
			// ChoiceList keeps the selected row visible within the supplied
			// viewport.
			chat.ChoiceListRender(frame, rect, row.lines, row.selected, &form.Form, row.id)
		}
		rowY += height
	}
	if inner.Dy() > 1 {
		frame.RenderWidget(
			chat.StyledLine(
				"Enter attaches/accepts · Space edits · d removes · Tab moves",
				chat.Style{}.Foreground(chat.ThemePalette().Muted),
			),
			uv.Rect(inner.Min.X, inner.Max.Y-2, inner.Dx(), 1),
		)
	}
	footerHeight := 0
	if inner.Dy() > 0 {
		footerHeight = 1
	}
	footer := uv.Rect(inner.Min.X, max(inner.Max.Y-1, 0), inner.Dx(), footerHeight)
	form.RenderActions(frame, footer, []chat.ButtonSpec[ContainerEditFocus]{
		{ID: ContainerEditFocusCancel, Label: "Cancel", Enabled: true},
		{ID: ContainerEditFocusSave, Label: "Save", Enabled: true},
	})
	// The completion popup hangs over the rows below the attached directory,
	// so it is drawn again once those rows are on the screen.
	if sourceField != nil && focused == ContainerEditFocusSource {
		chat.PathFieldRender(frame, *sourceField, &editor.source, &form.Form, ContainerEditFocusSource)
	}
	if accessField != nil && editor.accessCombo.IsOpen(ContainerEditFocusAccess) {
		renderAccessCombo(frame, inner, *accessField, editor.access, choices, &editor.accessCombo, true, &form.Form, ContainerEditFocusAccess)
	}
	form.EndFrame(ContainerEditFocusCpus)
}

func (d *DashboardState) beginContainerEdit() {
	session := d.selectedContainerSession()
	if session == nil {
		d.notices.Set("Container size and mounts apply to container targets only.")
		return
	}
	var suggestions []string
	if target, ok := d.config.Get().Targets[session.TargetTemplateID]; ok {
		if host, ok := core.MountHistoryHost(target); ok {
			if history, ok := d.state.Get().MountHistory.Get(host); ok {
				suggestions = slices.Clone(history)
			}
		}
	}
	var cpus, memory string
	if container, ok := session.ResourceAllocation.(core.SessionResourceAllocationContainer); ok {
		cpus = strconv.FormatUint(container.Cpus, 10)
		memory = memoryGibText(container.MemoryBytes)
	}
	var buildCache *string
	if session.BuildCache != nil {
		directory := session.BuildCache.Directory
		buildCache = &directory
	}
	editor := &ContainerEditor{
		sessionID:          session.ID,
		sessionName:        session.ListedTitle(),
		limits:             d.hostLimits(session.TargetTemplateID),
		originalAllocation: session.ResourceAllocation,
		cpus:               chat.TextInputFromValue(cpus),
		memory:             chat.TextInputFromValue(memory),
		mounts:             slices.Clone(session.AdditionalMounts),
		suggestions:        suggestions,
		source:             chat.NewPathInput(),
		destination:        chat.NewPathInput(),
		access:             core.MountAccessRo,
		form:               chat.NewDialog[ContainerEditFocus](),
		buildCache:         buildCache,
	}
	editor.prepare()
	d.mode = ModeEditContainer{editor}
}

// ContainerPathResolved takes the host's resolution of the attached
// directory typed for context: the resolved path, or the error that stopped
// it.
func (d *DashboardState) ContainerPathResolved(context string, path string, err error) {
	if d.PathInputContext() != context {
		return
	}
	mode, ok := d.mode.(ModeEditContainer)
	if !ok {
		return
	}
	editor := mode.ContainerEditor
	if err != nil {
		message := err.Error()
		editor.error = &message
		return
	}
	draft := editor.source
	editor.source.SetValue(path)
	editor.error = editor.addMount()
	if editor.error != nil {
		editor.source = draft
	}
	editor.prepare()
}

func (d *DashboardState) handleContainerEditEvent(event tea.Msg, editor ContainerEditor) DashboardAction {
	// Removing a directory is a domain operation on the focused list.
	if !editor.form.ConfirmationOpen() {
		if press, ok := event.(tea.KeyPressMsg); ok && !press.IsRepeat {
			code, modifiers := chat.NormalizeKey(chat.KeyPressCode(press), press.Mod)
			focused := editor.focused()
			if modifiers == 0 && (code == tea.KeyDelete || code == 'd') &&
				(focused == ContainerEditFocusMounts || focused == ContainerEditFocusSuggestions) {
				editor.removeSelected()
				editor.prepare()
				d.mode = ModeEditContainer{&editor}
				return DashboardActionNone{}
			}
		}
	}
	result := editor.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	changedStructure := false
	action, routed, consumed := routePathCompletion(d, &editor, result.Action)
	if consumed {
		editor.prepare()
		d.mode = ModeEditContainer{&editor}
		return action
	}
	switch interaction := editor.accessCombo.Route(routed).(type) {
	case chat.InteractionCancel[ContainerEditFocus]:
		d.CancelModal()
		return DashboardActionNone{}
	case chat.InteractionEdit[ContainerEditFocus]:
		// A path field's own apply closes its completion popup when the edit
		// changes the text it was completing.
		changed := false
		switch interaction.ID {
		case ContainerEditFocusSource:
			changed = chat.PathFieldApply(&editor.source, interaction.Edit) == chat.EditOutcomeChanged
		case ContainerEditFocusDestination:
			changed = chat.PathFieldApply(&editor.destination, interaction.Edit) == chat.EditOutcomeChanged
		default:
			if field := editor.fieldMut(interaction.ID); field != nil {
				changed = chat.TextFieldApply(field, interaction.Edit) == chat.EditOutcomeChanged
			}
		}
		if changed {
			editor.error = nil
			changedStructure = true
		}
	case chat.InteractionSelect[ContainerEditFocus]:
		switch interaction.ID {
		case ContainerEditFocusMounts:
			editor.mountIndex = interaction.Index
		case ContainerEditFocusSuggestions:
			editor.suggestionIndex = interaction.Index
		}
	case chat.InteractionToggle[ContainerEditFocus]:
		if interaction.ID == ContainerEditFocusMounts {
			editor.editSelectedMount()
			changedStructure = true
		}
	case chat.InteractionComboBoxCommit[ContainerEditFocus]:
		if interaction.ID == ContainerEditFocusAccess {
			if choices := accessChoices(false); interaction.Index >= 0 && interaction.Index < len(choices) {
				editor.access = choices[interaction.Index]
			}
			changedStructure = true
		}
	case chat.InteractionComboBoxDismiss[ContainerEditFocus]:
		if interaction.ID == ContainerEditFocusAccess {
			changedStructure = true
		}
	case chat.InteractionActivate[ContainerEditFocus]:
		switch interaction.ID {
		case ContainerEditFocusCancel:
			d.CancelModal()
			return DashboardActionNone{}
		case ContainerEditFocusAccess:
			selected := accessIndex(accessChoices(false), editor.access)
			editor.accessCombo.Open(ContainerEditFocusAccess, selected)
			changedStructure = true
		case ContainerEditFocusSuggestions:
			editor.takeSuggestion()
			editor.error = nil
		case ContainerEditFocusSource, ContainerEditFocusDestination:
			needsHome, err := core.NeedsHome(strings.TrimSpace(editor.source.Value()))
			if err != nil {
				message := err.Error()
				editor.error = &message
				d.mode = ModeEditContainer{&editor}
				return DashboardActionNone{}
			}
			if needsHome {
				action := DashboardActionResolveContainerPath{
					SessionID: editor.sessionID,
					Source:    editor.source.String(),
				}
				message := "Resolving attached directory…"
				editor.error = &message
				d.mode = ModeEditContainer{&editor}
				return action
			}
			editor.error = editor.addMount()
			changedStructure = true
		case ContainerEditFocusCpus, ContainerEditFocusMemory, ContainerEditFocusMounts, ContainerEditFocusSave:
			action, err := editor.save()
			if err == nil {
				d.CancelModal()
				return action
			}
			message := err.Error()
			editor.error = &message
		}
	}
	// An open combobox's cursor lives in the declared control kind, so it is
	// refreshed on every event while the list is open.
	if _, open := editor.accessCombo.OpenID(); changedStructure || open {
		editor.prepare()
	}
	d.mode = ModeEditContainer{&editor}
	return DashboardActionNone{}
}

// focusedPathInput is CompletesPaths::focused_path_input. The attached
// directory lives on the session's own target host; the container
// destination does not exist yet, so it does not complete.
func (e *ContainerEditor) focusedPathInput(dashboard *DashboardState) (*chat.PathInput, core.CompletionHost, core.CompletionKind, bool) {
	if e.focused() != ContainerEditFocusSource {
		return nil, nil, 0, false
	}
	session, ok := dashboard.state.Get().Sessions.Get(e.sessionID)
	if !ok {
		return nil, nil, 0, false
	}
	return &e.source, core.CompletionHostTarget{ID: session.TargetTemplateID}, core.CompletionKindDirectories, true
}

// dismissUnfocusedCompletions is CompletesPaths::dismiss_unfocused_completions.
func (e *ContainerEditor) dismissUnfocusedCompletions() {
	if e.focused() != ContainerEditFocusSource {
		e.source.DismissCompletion()
	}
}

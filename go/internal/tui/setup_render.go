// Port of mj-tui/src/setup/render.rs.

package tui

import (
	"fmt"
	"math"
	"slices"
	"strings"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// settingGutter is the indent in front of every setting row.
const settingGutter = "  "

// preferredSize is the dialog size the draft's widest and tallest page
// needs, clamped to what Settings ever draws.
func preferredSize(draft any) SetupSize {
	maxWidth := 0
	maxHeight := 20
	var walk func(path []string)
	walk = func(path []string) {
		value, ok := setupLookup(draft, path)
		if !ok {
			return
		}
		paths := client.VisiblePaths(path, value)
		maxWidth = max(maxWidth, chat.RawLine(client.Breadcrumb(path, draft)).Width())
		// Rust's height is a u16 and saturates there.
		maxHeight = max(maxHeight, min(min(len(client.PagePlan(path, paths)), math.MaxUint16)+10, math.MaxUint16))
		for _, childPath := range paths {
			key := ""
			if len(childPath) > 0 {
				key = childPath[len(childPath)-1]
			}
			parent := slices.Clone(childPath[:max(len(childPath)-1, 0)])
			child, ok := setupLookup(draft, childPath)
			if !ok {
				continue
			}
			parentValue, ok := setupLookup(draft, parent)
			if !ok {
				parentValue = value
			}
			name := client.RowLabel(parent, parentValue, key, child)
			summary := rowSummary(parent, key, child, draft, nil)
			if _, isBool := setupValueBool(child); !setupValueIsObject(child) && !setupValueIsArray(child) && !isBool &&
				len(client.Choices(client.StoragePath(childPath), draft)) == 0 {
				if runes := []rune(summary); len(runes) > 24 {
					summary = string(runes[:24])
				}
			}
			// The gutter, the name, the gap the value is pushed away by, and
			// the trailing column the row ends with.
			line := settingGutter + name + "    " + summary + " "
			maxWidth = max(maxWidth, chat.RawLine(line).Width())
			// `interface` resolves to the draft root, which holds most of the
			// settings its page gathers; the prefix has its own nested
			// mapping.
			if (setupValueIsObject(child) || setupValueIsArray(child)) && !slices.Equal(childPath, []string{"review"}) {
				walk(childPath)
			}
		}
	}
	walk([]string{})
	// The page's own actions stack in a column at its right edge, so the
	// dialog is as wide as the widest page body plus that column. Back and
	// the commit sit in the footer row instead and take no width here.
	column := 0
	for _, label := range []string{"Add", "Create", "Remove", "Detect profiles", "Detect runtimes", "Use default", "Apply"} {
		column = max(column, chat.RawLine(label).Width()+4)
	}
	column += chat.ButtonColumnBodyGap
	maxWidth += column
	return SetupSize{
		// The body, its gap to the column, the column, and the inner margin.
		width: min(max(min(maxWidth+4, math.MaxUint16), 64), 96),
		// One extra row over the page body for the footer action row.
		height: min(max(min(maxHeight+1, math.MaxUint16), 21), 33),
	}
}

// setupWarning is the style of a refused value or a missing runtime.
func setupWarning() chat.Style {
	return chat.Style{}.Foreground(chat.ThemePalette().Warning)
}

// renderSetup draws the Settings dialog.
func renderSetup(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, dialog *SetupDialog, surfaces *chat.FrameSurfaces) {
	popup := centeredModalFixed(frame, surfaces, dialog.preferredWidth, dialog.preferredHeight, area)
	if review := dialog.reviewEditor; review != nil {
		renderReviewSettings(frame, popup, review, dialog.saving)
		return
	}
	inner := chat.RectInner(popup, chat.Margin{Horizontal: 2, Vertical: 1})
	if dialog.managedSetup != nil {
		renderManagedSetup(frame, popup, inner, dialog)
		return
	}
	if dialog.search != nil {
		renderSetupSearch(frame, popup, inner, dialog)
		return
	}
	if dialog.report != nil {
		renderReport(frame, popup, inner, dialog)
		return
	}
	form := &dialog.form
	form.BeginFrame()
	_, comboOpen := dialog.combo.OpenID()
	title := dismissibleModalTitle(&form.Form, popup, "Settings", chat.Title(true), !dialog.saving && !comboOpen)
	frame.RenderWidget(
		chat.Modal().WithTitle(title).WithTitleBottom(chat.RawLine(" ↑/↓ move · Enter apply/open · Tab moves · Esc back ")),
		popup,
	)
	top := inner.Min.Y
	if len(dialog.path) > 0 {
		frame.RenderWidget(
			chat.StyledLine(client.Breadcrumb(dialog.path, dialog.draft), chat.Title(true)),
			uv.Rect(inner.Min.X, top, inner.Dx(), 1),
		)
		top++
	}
	help := chat.NewParagraph(chat.RawText(client.PageHelp(dialog.path, dialog.draft))).
		WithWrap(chat.Wrap{Trim: false}).
		WithStyle(chat.Muted())
	helpRows := min(max(min(help.LineCount(inner.Dx()), math.MaxUint16), 2), max(inner.Dy()/3, 2))
	frame.RenderWidget(help, uv.Rect(inner.Min.X, top, inner.Dx(), helpRows))
	top += helpRows + 1
	footerRow := chat.DialogShellLayout(inner, 0).Actions
	noticeRows := 0
	if dialog.notice != nil {
		count := chat.NewParagraph(chat.RawText(*dialog.notice)).WithWrap(chat.Wrap{Trim: false}).LineCount(inner.Dx())
		noticeRows = max(min(min(count, math.MaxUint16), max(max(footerRow.Min.Y-top, 0)-2, 0)), 1)
	}
	band := uv.Rect(inner.Min.X, top, inner.Dx(), max(footerRow.Min.Y-(top+noticeRows), 0))
	pageActions := dialog.pageActions()
	split := splitPage(form, band, pageActions)
	body, column := split.Body, split.Actions
	var expanded *uv.Rectangle
	var pathPopup *setupPathPopup
	var initial SetupControl
	if editor := dialog.editor; editor != nil {
		frame.RenderWidget(chat.RawLine("Name for the new entry"), uv.Rect(body.Min.X, body.Min.Y, body.Dx(), 1))
		fieldHeight := 0
		if body.Dy() > 1 {
			fieldHeight = 1
		}
		chat.TextFieldRender(frame, uv.Rect(body.Min.X, body.Min.Y+1, body.Dx(), fieldHeight), editor.input.text(), &form.Form, SetupControlField)
		initial = SetupControlField
	} else {
		bounds := uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), max(footerRow.Min.Y-inner.Min.Y, 0))
		expanded, pathPopup = renderControls(frame, bounds, body, dashboard, dialog, form)
		initial = SetupControlSave
		if isSubagentCategoryPage(dialog.path) {
			initial = SetupControlAdd
		}
		for _, control := range dialog.descriptors {
			if _, readOnly := control.kind.(SettingKindReadOnly); !readOnly {
				initial = control.id
				break
			}
		}
	}
	if comboOpen {
		form.RenderActionsStackedInert(frame, column, pageActions, chat.ColumnAlignRight)
		form.RenderActionsInert(frame, footerRow, dialog.footerActions())
	} else {
		form.RenderActionsStacked(frame, column, pageActions, chat.ColumnAlignRight)
		form.RenderActions(frame, footerRow, dialog.footerActions())
	}
	if dialog.notice != nil {
		frame.RenderWidget(
			chat.NewParagraph(chat.RawText(*dialog.notice)).WithWrap(chat.Wrap{Trim: false}),
			uv.Rect(inner.Min.X, max(footerRow.Min.Y-noticeRows, 0), inner.Dx(), noticeRows),
		)
	}
	// Popups draw last, above both the page controls and its action column.
	if openID, ok := dialog.combo.OpenID(); ok {
		if control, ok := dialog.descriptor(openID); ok {
			if _, isChoices := control.kind.(SettingKindChoices); isChoices && expanded != nil {
				renderCombo(frame, inner, *expanded, control, dialog, form, true)
			}
		}
	}
	if pathPopup != nil {
		if control, ok := dialog.descriptor(pathPopup.id); ok {
			if input := dialog.fieldOf(control.path).input.pathInput(); input != nil {
				bounds := uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), max(footerRow.Min.Y-inner.Min.Y, 0))
				chat.PathFieldRenderWithin(frame, bounds, pathPopup.field, input, &form.Form, pathPopup.id)
			}
		}
	}
	form.EndFrame(initial)
}

// fieldOf is Rust's `fields[&path]`: the buffer a declared field always has.
func (d *SetupDialog) fieldOf(path []string) *PendingField {
	field, ok := d.fields.get(path)
	if !ok {
		panic(fmt.Sprintf("no field buffer for %v", path))
	}
	return field
}

func renderCombo(frame *chat.Frame, bounds uv.Rectangle, field uv.Rectangle, control *SettingControl, dialog *SetupDialog, form *chat.Dialog[SetupControl], expanded bool) {
	choices, ok := control.kind.(SettingKindChoices)
	if !ok {
		return
	}
	committed := dialog.choicePosition(control.path, choices.values)
	selected := dialog.combo.Selection(control.id, committed)
	rows := make([]chat.Line, 0, len(choices.values))
	for _, value := range choices.values {
		rows = append(rows, chat.RawLine(dialog.choiceLabel(control.path, value)))
	}
	value := ""
	if selected >= 0 && selected < len(choices.values) {
		value = dialog.choiceLabel(control.path, choices.values[selected])
	}
	openID, open := dialog.combo.OpenID()
	chat.ComboBoxRender(
		frame,
		bounds,
		field,
		value,
		rows,
		selected,
		expanded,
		!open || openID == control.id,
		" values · ↑/↓ select · Tab/Enter accept ",
		chat.PopupSideBelow,
		chat.ComboBoxDefaultMaxVisibleRows,
		&form.Form,
		control.id,
	)
}

// setupRowBlock is one block of a settings page: a heading or gap
// (index < 0), or a setting row and the notes under it.
type setupRowBlock struct {
	start int
	index int
	notes []chat.Line
}

// renderControls draws the page's setting rows and returns where an
// expanded combobox and a path completion popup are anchored, to draw them
// last.
func renderControls(frame *chat.Frame, bounds uv.Rectangle, body uv.Rectangle, dashboard *DashboardState, dialog *SetupDialog, form *chat.Dialog[SetupControl]) (*uv.Rectangle, *setupPathPopup) {
	if isSubagentCategoryPage(dialog.path) {
		return renderSubagentControls(frame, bounds, body, dialog, form)
	}
	var expanded *uv.Rectangle
	var pathPopup *setupPathPopup
	// Layout entries refer to descriptors; no separate interactive row
	// identity exists.
	var blocks []setupRowBlock
	at := 0
	paths := make([][]string, 0, len(dialog.descriptors))
	for _, control := range dialog.descriptors {
		paths = append(paths, slices.Clone(control.path))
	}
	for _, row := range client.PagePlan(dialog.path, paths) {
		switch row := row.(type) {
		case client.PageRowGap:
			blocks = append(blocks, setupRowBlock{start: at, index: -1, notes: []chat.Line{chat.RawLine("")}})
			at++
		case client.PageRowHeading:
			blocks = append(blocks, setupRowBlock{start: at, index: -1, notes: []chat.Line{
				chat.StyledLine("  "+row.Value, chat.Muted().Add(chat.ModifierBold)),
			}})
			at++
		case client.PageRowSetting:
			index := row.Value
			control := &dialog.descriptors[index]
			key := control.path[len(control.path)-1]
			var notes []chat.Line
			if field, ok := dialog.fields.get(control.path); ok && field.err != nil {
				notes = append(notes, chat.WrapLines([]chat.Line{chat.StyledLine(*field.err, setupWarning())}, body.Dx())...)
			}
			if client.IsBuildCacheField(control.path, "enabled") {
				if preview := dialog.currentBuildCachePreview(); preview != nil {
					effective := "Effective: On"
					if preview.OffReason != nil {
						effective = "Effective: Off"
					}
					notes = append(notes, chat.StyledLine(effective, chat.Muted()))
				}
				if reason, ok := dialog.mbxUnavailable(); ok {
					notes = append(notes, chat.WrapLines([]chat.Line{chat.StyledLine("mbx: "+reason, chat.Muted())}, body.Dx())...)
				}
			}
			if key == "archive_after_days" {
				if estimate, ok := dialog.archiveSpaceEstimate(); ok {
					notes = append(notes, chat.StyledLine(estimate, chat.Muted()))
				}
			}
			if slices.Equal(dialog.path, []string{"machines"}) {
				if note, ok := machineWithoutRuntimeNote(dashboard, dialog, key); ok {
					wrapped := chat.WrapLines([]chat.Line{chat.StyledLine(note, setupWarning())}, body.Dx())
					notes = append(notes, wrapped[:min(len(wrapped), machineNoteLines)]...)
				}
			}
			if form.IsFocused(control.id) {
				help := client.Help(control.path)
				if help != "Enter opens or edits a setting. Changes stay in this draft until you save." &&
					help != client.PageHelp(dialog.path, dialog.draft) {
					notes = append(notes, chat.WrapLines([]chat.Line{chat.StyledLine(help, chat.Muted())}, body.Dx())...)
				}
			}
			blocks = append(blocks, setupRowBlock{start: at, index: index, notes: notes})
			at += 1 + len(notes)
		}
	}
	tail := dialog.buildCachePolicyLines()
	if dialog.buildCacheUserManaged() {
		tail = append(tail, "This machine's mbx installation owns its budgets. Mjolnir does not modify them.")
	}
	if stats, ok := dialog.buildCacheStatsLine(); ok {
		tail = append(tail, stats)
	}
	tailLines := make([]chat.Line, 0, len(tail))
	for _, line := range tail {
		tailLines = append(tailLines, chat.RawLine(line))
	}
	tailLines = chat.WrapLines(tailLines, body.Dx())
	focused, hasFocused := form.Focused()
	var focusedBlock *chat.RowRange
	for _, block := range blocks {
		if block.index >= 0 && hasFocused && dialog.descriptors[block.index].id == focused {
			focusedBlock = &chat.RowRange{Start: block.start, End: block.start + 1 + len(block.notes)}
			break
		}
	}
	viewport := form.Viewport(body, at+len(tailLines), focusedBlock)
	openID, comboOpen := dialog.combo.OpenID()
	for _, block := range blocks {
		if block.index < 0 {
			for i, note := range block.notes {
				frame.RenderWidget(note, viewport.Row(block.start+i, 1))
			}
			continue
		}
		control := &dialog.descriptors[block.index]
		enabled := !comboOpen || openID == control.id
		key := control.path[len(control.path)-1]
		value, ok := setupLookup(dialog.draft, control.path)
		if !ok {
			panic("a drawn setting exists")
		}
		parent := control.path[:len(control.path)-1]
		parentValue, ok := setupLookup(dialog.draft, parent)
		if !ok {
			panic("a drawn setting's page exists")
		}
		label := client.RowLabel(parent, parentValue, key, value)
		row := viewport.Row(block.start, 1)
		labelWidth := min(body.Dx()/2, 34)
		labelArea := uv.Rect(row.Min.X, row.Min.Y, min(labelWidth, row.Dx()), row.Dy())
		fieldArea := uv.Rect(row.Min.X+labelArea.Dx(), row.Min.Y, max(row.Dx()-labelArea.Dx(), 0), row.Dy())
		switch control.kind.(type) {
		case SettingKindPage:
			chat.ButtonRenderAligned(frame, row, label, enabled, &form.Form, control.id, chat.AlignmentLeft)
			var automatic *string
			if key == "build_cache" {
				if unavailable, ok := dialog.buildCacheRowUnavailable(); ok {
					automatic = &unavailable
				}
			}
			summary := rowSummary(dialog.path, key, value, dialog.draft, automatic)
			if slices.Equal(dialog.path, []string{"profiles"}) && dialog.savedIdentityMatches(key) {
				if status, ok := dashboard.profileLoginStatus(key); ok {
					summary = fmt.Sprintf("%s  %s  ›", strings.TrimSuffix(summary, "  ›"), status.Label())
				}
			}
			if slices.Equal(dialog.path, []string{"machines"}) {
				if _, ok := machineWithoutRuntimeNote(dashboard, dialog, key); ok {
					summary = "No runtime  ›"
				}
			}
			frame.RenderWidget(chat.StyledLine(summary, chat.Muted()), fieldArea)
		case SettingKindCheckbox:
			checked, _ := setupValueBool(value)
			chat.CheckboxRender(frame, row, label, checked, enabled, &form.Form, control.id)
		case SettingKindChoices:
			frame.RenderWidget(chat.RawLine(label), labelArea)
			renderCombo(frame, bounds, fieldArea, control, dialog, form, false)
			if dialog.combo.IsOpen(control.id) {
				area := fieldArea
				expanded = &area
			}
		case SettingKindField:
			frame.RenderWidget(chat.RawLine(label), labelArea)
			field := dialog.fieldOf(control.path)
			if !enabled {
				frame.RenderWidget(chat.RawLine(field.input.String()), fieldArea)
			} else if input := field.input.pathInput(); input != nil {
				chat.PathFieldRenderWithin(frame, bounds, fieldArea, input, &form.Form, control.id)
			} else {
				chat.TextFieldRender(frame, fieldArea, field.input.text(), &form.Form, control.id)
			}
			if input := field.input.pathInput(); enabled && form.IsFocused(control.id) && input != nil &&
				chat.PathFieldPopupRows(input) > 0 {
				pathPopup = &setupPathPopup{id: control.id, field: fieldArea}
			}
			if field.input.text().IsEmpty() {
				if hint, ok := dialog.emptyFieldHint(control.path); ok {
					frame.RenderWidget(chat.StyledLine(hint, chat.Muted().Add(chat.ModifierItalic)), fieldArea)
				}
			}
		case SettingKindManagedHarness:
			panic("managed chooser renders separately")
		case SettingKindRemoveEntry:
			panic("subagent remove renders in render_subagent_controls")
		case SettingKindReadOnly:
			frame.RenderWidget(chat.StyledLine(label, chat.Muted()), labelArea)
			summary, ok := dialog.buildCacheAutomaticLabel(key)
			if !ok {
				summary = rowSummary(dialog.path, key, value, dialog.draft, nil)
			}
			frame.RenderWidget(chat.StyledLine(summary, chat.Muted()), fieldArea)
		}
		for i, note := range block.notes {
			frame.RenderWidget(note, viewport.Row(block.start+1+i, 1))
		}
	}
	for i, note := range tailLines {
		frame.RenderWidget(note.WithStyle(chat.Muted()), viewport.Row(at+i, 1))
	}
	return expanded, pathPopup
}

// setupPathPopup is the focused path field whose completion popup draws
// last: its control and its field area (Rust's `(SetupControl, Rect)`).
type setupPathPopup struct {
	id    SetupControl
	field uv.Rectangle
}

// SubagentRowControls are the controls of one subagent entry.
type SubagentRowControls struct {
	index  int
	model  *SettingControl
	effort *SettingControl
	remove *SettingControl
}

func renderSubagentValue(frame *chat.Frame, bounds uv.Rectangle, fieldArea uv.Rectangle, control *SettingControl, dialog *SetupDialog, form *chat.Dialog[SetupControl], expanded **uv.Rectangle) {
	openID, open := dialog.combo.OpenID()
	enabled := !open || openID == control.id
	switch control.kind.(type) {
	case SettingKindChoices:
		renderCombo(frame, bounds, fieldArea, control, dialog, form, false)
		if dialog.combo.IsOpen(control.id) {
			area := fieldArea
			*expanded = &area
		}
	case SettingKindField:
		field := dialog.fieldOf(control.path)
		switch input := field.input.pathInput(); {
		case !enabled:
			frame.RenderWidget(chat.RawLine(field.input.String()), fieldArea)
		case input != nil:
			chat.PathFieldRenderWithin(frame, bounds, fieldArea, input, &form.Form, control.id)
		default:
			chat.TextFieldRender(frame, fieldArea, field.input.text(), &form.Form, control.id)
		}
	}
}

// setupSubagentBlock is one inline subagent row and the notes under it.
type setupSubagentBlock struct {
	start  int
	height int
	entry  *SubagentRowControls
	notes  []chat.Line
}

// setupStackedSubagentBlock is one subagent entry stacked over several rows
// on a narrow body: model, then effort when it has one, then Remove.
type setupStackedSubagentBlock struct {
	modelRow  int
	effortRow *int
	removeRow int
	entry     *SubagentRowControls
	notes     []chat.Line
}

func renderSubagentControls(frame *chat.Frame, bounds uv.Rectangle, body uv.Rectangle, dialog *SetupDialog, form *chat.Dialog[SetupControl]) (*uv.Rectangle, *setupPathPopup) {
	count := 0
	if entries, ok := setupValueArray(dialog.current()); ok {
		count = len(entries)
	}
	if count == 0 {
		frame.RenderWidget(
			chat.NewParagraph(chat.RawText("  No agents configured. Choose Add to add one.")).
				WithWrap(chat.Wrap{Trim: false}).
				WithStyle(chat.Muted()),
			body,
		)
		return nil, nil
	}
	rows := make([]SubagentRowControls, 0, count)
	for index := 0; index < count; index++ {
		var model, effort, remove *SettingControl
		for i := range dialog.descriptors {
			control := &dialog.descriptors[i]
			if row, ok := subagentRowIndex(control.path); !ok || row != index {
				continue
			}
			last := control.path[len(control.path)-1]
			if _, isRemove := control.kind.(SettingKindRemoveEntry); isRemove {
				remove = control
			} else if last == "model" {
				model = control
			} else if last == "effort" {
				effort = control
			}
		}
		if model != nil && remove != nil {
			rows = append(rows, SubagentRowControls{index: index, model: model, effort: effort, remove: remove})
		}
	}
	focused, hasFocused := form.Focused()
	isFocused := func(id SetupControl) bool { return hasFocused && focused == id }
	_, comboOpen := dialog.combo.OpenID()
	var expanded *uv.Rectangle
	errorNotes := func(path []string) []chat.Line {
		if field, ok := dialog.fields.get(path); ok && field.err != nil {
			return chat.WrapLines([]chat.Line{chat.StyledLine(*field.err, setupWarning())}, body.Dx())
		}
		return nil
	}
	removeButton := func(area uv.Rectangle, id SetupControl, label string) {
		if comboOpen {
			form.RenderActionsInert(frame, area, []chat.ButtonSpec[SetupControl]{setupButton(id, label, false)})
		} else {
			form.RenderActions(frame, area, []chat.ButtonSpec[SetupControl]{setupButton(id, label, true)})
		}
	}
	inline := body.Dx() >= 60
	if inline {
		const indexWidth = 4
		const removeWidth = 10
		const effortWidth = 28
		const gap = 1
		modelWidth := max(body.Dx()-(indexWidth+effortWidth+removeWidth+gap*2), 0)
		blocks := make([]setupSubagentBlock, 0, len(rows))
		at := 1 // line 0 is the column header
		for i := range rows {
			entry := &rows[i]
			notes := errorNotes(entry.model.path)
			if entry.effort != nil {
				notes = append(notes, errorNotes(entry.effort.path)...)
			}
			height := 1 + len(notes)
			blocks = append(blocks, setupSubagentBlock{start: at, height: height, entry: entry, notes: notes})
			at += height
		}
		var focusedBlock *chat.RowRange
		for _, block := range blocks {
			hit := isFocused(block.entry.model.id) ||
				(block.entry.effort != nil && isFocused(block.entry.effort.id)) ||
				isFocused(block.entry.remove.id)
			if hit {
				focusedBlock = &chat.RowRange{Start: block.start, End: block.start + block.height}
				break
			}
		}
		viewport := form.Viewport(body, at, focusedBlock)
		header := viewport.Row(0, 1)
		headerStyle := chat.Muted().Add(chat.ModifierBold)
		frame.RenderWidget(
			chat.StyledLine("Model", headerStyle),
			uv.Rect(header.Min.X+indexWidth, header.Min.Y, min(modelWidth, max(header.Dx()-indexWidth, 0)), header.Dy()),
		)
		effortX := header.Min.X + indexWidth + modelWidth + gap
		frame.RenderWidget(
			chat.StyledLine("Effort", headerStyle),
			uv.Rect(effortX, header.Min.Y, min(effortWidth, max(header.Max.X-effortX, 0)), header.Dy()),
		)
		for _, block := range blocks {
			row := viewport.Row(block.start, 1)
			indexArea := uv.Rect(row.Min.X, row.Min.Y, min(indexWidth, row.Dx()), row.Dy())
			modelArea := uv.Rect(row.Min.X+indexArea.Dx(), row.Min.Y, min(modelWidth, max(row.Dx()-indexArea.Dx(), 0)), row.Dy())
			effortX := modelArea.Max.X + gap
			effortArea := uv.Rect(effortX, row.Min.Y, min(effortWidth, max(row.Max.X-effortX, 0)), row.Dy())
			removeX := effortArea.Max.X + gap
			removeArea := uv.Rect(removeX, row.Min.Y, min(removeWidth, max(row.Max.X-removeX, 0)), row.Dy())
			frame.RenderWidget(chat.StyledLine(fmt.Sprintf("%3d ", block.entry.index+1), chat.Muted()), indexArea)
			renderSubagentValue(frame, bounds, modelArea, block.entry.model, dialog, form, &expanded)
			if effort := block.entry.effort; effort != nil {
				renderSubagentValue(frame, bounds, effortArea, effort, dialog, form, &expanded)
			} else {
				frame.RenderWidget(chat.StyledLine("—", chat.Muted()), effortArea)
			}
			removeButton(removeArea, block.entry.remove.id, "Remove")
			for i, note := range block.notes {
				frame.RenderWidget(note, viewport.Row(block.start+1+i, 1))
			}
		}
		return expanded, nil
	}
	blocks := make([]setupStackedSubagentBlock, 0, len(rows))
	at := 0
	for position := range rows {
		entry := &rows[position]
		notes := errorNotes(entry.model.path)
		modelRow := at
		at += 1 + len(notes)
		var effortRow *int
		if entry.effort != nil {
			row := at
			effortRow = &row
			at++
		}
		removeRow := at
		at++
		if position+1 < len(rows) {
			at++
		}
		blocks = append(blocks, setupStackedSubagentBlock{
			modelRow:  modelRow,
			effortRow: effortRow,
			removeRow: removeRow,
			entry:     entry,
			notes:     notes,
		})
	}
	var focusedBlock *chat.RowRange
	for _, block := range blocks {
		switch {
		case isFocused(block.entry.model.id):
			focusedBlock = &chat.RowRange{Start: block.modelRow, End: block.modelRow + 1}
		case block.entry.effort != nil && block.effortRow != nil && isFocused(block.entry.effort.id):
			focusedBlock = &chat.RowRange{Start: *block.effortRow, End: *block.effortRow + 1}
		case isFocused(block.entry.remove.id):
			focusedBlock = &chat.RowRange{Start: block.removeRow, End: block.removeRow + 1}
		default:
			continue
		}
		break
	}
	viewport := form.Viewport(body, at, focusedBlock)
	labelWidth := min(12, body.Dx())
	labelled := func(rowIndex int, label string, control *SettingControl) {
		row := viewport.Row(rowIndex, 1)
		labelArea := uv.Rect(row.Min.X, row.Min.Y, min(labelWidth, row.Dx()), row.Dy())
		fieldArea := uv.Rect(row.Min.X+labelArea.Dx(), row.Min.Y, max(row.Dx()-labelArea.Dx(), 0), row.Dy())
		frame.RenderWidget(chat.RawLine(label), labelArea)
		renderSubagentValue(frame, bounds, fieldArea, control, dialog, form, &expanded)
	}
	for _, block := range blocks {
		labelled(block.modelRow, fmt.Sprintf("  %d model", block.entry.index+1), block.entry.model)
		for i, note := range block.notes {
			frame.RenderWidget(note, viewport.Row(block.modelRow+1+i, 1))
		}
		if block.entry.effort != nil && block.effortRow != nil {
			labelled(*block.effortRow, fmt.Sprintf("  %d effort", block.entry.index+1), block.entry.effort)
		}
		removeButton(viewport.Row(block.removeRow, 1), block.entry.remove.id, fmt.Sprintf("Remove %d", block.entry.index+1))
	}
	return expanded, nil
}

func renderSetupSearch(frame *chat.Frame, popup uv.Rectangle, inner uv.Rectangle, dialog *SetupDialog) {
	search := dialog.search
	if search == nil {
		panic("an open search")
	}
	form := &dialog.form
	form.BeginFrame()
	title := dismissibleModalTitle(&form.Form, popup, "Settings", chat.Title(true), !dialog.saving)
	frame.RenderWidget(
		chat.Modal().WithTitle(title).WithTitle(
			chat.StyledLine(fmt.Sprintf(" %s ", counted(len(search.matches), "setting", "settings")), chat.Muted()).RightAligned(),
		),
		popup,
	)
	frame.RenderWidget(
		chat.NewParagraph(chat.RawText("Search every setting by name, by what it does, or by its value. Esc returns to the list.")).
			WithWrap(chat.Wrap{Trim: false}).
			WithStyle(chat.Muted()),
		uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), 2),
	)
	query := uv.Rect(inner.Min.X, inner.Min.Y+2, inner.Dx(), 1)
	chat.TextFieldRender(frame, query, &search.input, &form.Form, SetupControlSearch)
	if search.input.IsEmpty() {
		frame.RenderWidget(chat.StyledLine("Type to filter…", chat.Muted().Add(chat.ModifierItalic)), query)
	}
	footerRow := chat.DialogShellLayout(inner, 0).Actions
	top := inner.Min.Y + 4
	results := uv.Rect(inner.Min.X, top, inner.Dx(), max(footerRow.Min.Y-top, 1))
	if len(search.matches) == 0 {
		frame.RenderWidget(chat.RawLine("No matching setting"), results)
		form.Register(SetupControlResults, chat.ControlKindChoiceList{Len: 0, Selected: 0}, results, false)
	} else {
		rows := make([]chat.Line, 0, len(search.matches))
		for _, index := range search.matches {
			if index >= 0 && index < len(search.entries) {
				rows = append(rows, setupSearchRow(&search.entries[index], results.Dx()))
			}
		}
		chat.ChoiceListRender(frame, results, rows, search.selected, &form.Form, SetupControlResults)
	}
	form.RenderActions(frame, footerRow, dialog.footerActions())
	form.EndFrame(SetupControlSearch)
}

// splitPage splits a settings page into its body and the stacked column of
// actions.
//
// A page with no actions of its own keeps the full width; splitting on an
// empty list would still surrender the body gap to an empty column.
func splitPage(form *chat.Dialog[SetupControl], area uv.Rectangle, actions []chat.ButtonSpec[SetupControl]) chat.ColumnSplit {
	if len(actions) == 0 {
		return chat.ColumnSplit{
			Body:    area,
			Actions: uv.Rect(area.Max.X, area.Min.Y, 0, area.Dy()),
		}
	}
	return form.SplitActions(area, actions)
}

func managedHarnessDescription(kind core.HarnessKind) string {
	switch kind {
	case core.HarnessKindCodex:
		return "ChatGPT subscription or an OpenAI-compatible provider"
	case core.HarnessKindClaude:
		return "Claude Pro, Max, Team, or an Anthropic API key"
	case core.HarnessKindKimi:
		return "Kimi Code subscription"
	case core.HarnessKindGrok:
		return "Grok Build subscription"
	case core.HarnessKindMuse:
		return "Muse account; container use is supported"
	case core.HarnessKindOpenCode:
		return "OpenCode provider login or an API key"
	case core.HarnessKindDsh:
		return "DeepSeek API key"
	case core.HarnessKindAntigravity:
		return "Google OAuth login"
	}
	panic("unknown harness kind " + string(kind))
}

func renderManagedSetup(frame *chat.Frame, popup uv.Rectangle, inner uv.Rectangle, dialog *SetupDialog) {
	form := &dialog.form
	form.BeginFrame()
	title := dismissibleModalTitle(&form.Form, popup, " Set up a managed agent ", chat.Title(true), !dialog.saving)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	intro := chat.NewParagraph(chat.RawText("Choose an agent. Mjolnir prepares its exact pinned version and opens its sign-in flow.")).
		WithWrap(chat.Wrap{Trim: false}).
		WithStyle(chat.Muted())
	introHeight := intro.LineCount(inner.Dx())
	frame.RenderWidget(intro, uv.Rect(inner.Min.X, inner.Min.Y, inner.Dx(), introHeight))
	footer := chat.DialogShellLayout(inner, 0).Actions
	guidance := chat.NewParagraph(chat.RawText("Where it runs is chosen when you create a session. Use a local Podman, Docker, or Apple container when you want the agent isolated from your normal host account.")).
		WithWrap(chat.Wrap{Trim: false}).
		WithStyle(chat.Muted())
	guidanceHeight := guidance.LineCount(inner.Dx())
	guidanceY := max(footer.Min.Y-(guidanceHeight+1), 0)
	frame.RenderWidget(guidance, uv.Rect(inner.Min.X, guidanceY, inner.Dx(), guidanceHeight))
	top := inner.Min.Y + introHeight + 1
	body := uv.Rect(inner.Min.X, top, inner.Dx(), max(guidanceY-top, 0))
	var focusedBlock *chat.RowRange
	for position, control := range dialog.descriptors {
		if form.IsFocused(control.id) {
			focusedBlock = &chat.RowRange{Start: position * 2, End: position*2 + 2}
			break
		}
	}
	viewport := form.Viewport(body, len(dialog.descriptors)*2, focusedBlock)
	for position, control := range dialog.descriptors {
		managed, ok := control.kind.(SettingKindManagedHarness)
		if !ok {
			continue
		}
		kind := managedSetupHarnesses[managed.index]
		chat.ButtonRenderAligned(frame, viewport.Row(position*2, 1), kind.DisplayName(), !dialog.saving, &form.Form, control.id, chat.AlignmentLeft)
		frame.RenderWidget(chat.StyledLine(managedHarnessDescription(kind), chat.Muted()), viewport.Row(position*2+1, 1))
	}
	form.RenderActions(frame, footer, dialog.actions())
	initial := SetupControlBack
	if len(dialog.descriptors) > 0 {
		initial = dialog.descriptors[0].id
	}
	form.EndFrame(initial)
}

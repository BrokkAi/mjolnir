// Port of mj-tui/src/setup/form.rs.

package tui

import (
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
)

// actions is the one action set for the current screen. Buttons appear only
// on the pages where they apply; the renderer, the inert mirror behind a
// choice popup, and prepare all read this list so they cannot drift apart.
func (d *SetupDialog) actions() []chat.ButtonSpec[SetupControl] {
	panic("unported: mj-tui/src/setup/form.rs actions")
}

func (d *SetupDialog) saveLabel() string {
	if d.saving {
		return "Saving…"
	}
	return "Save and Close"
}

// footerActions are the actions drawn in the dialog's footer row: the way
// back out of a page and the commit that closes the dialog.
func (d *SetupDialog) footerActions() []chat.ButtonSpec[SetupControl] {
	var footer []chat.ButtonSpec[SetupControl]
	for _, action := range d.actions() {
		if action.ID == SetupControlBack || action.ID == SetupControlSave {
			footer = append(footer, action)
		}
	}
	return footer
}

// pageActions are the actions that belong to the page itself, stacked in the
// column at the dialog's right edge.
func (d *SetupDialog) pageActions() []chat.ButtonSpec[SetupControl] {
	var page []chat.ButtonSpec[SetupControl]
	for _, action := range d.actions() {
		if action.ID != SetupControlBack && action.ID != SetupControlSave {
			page = append(page, action)
		}
	}
	return page
}

func (d *SetupDialog) noticeView() setupNoticeView {
	return setupNoticeView{
		path:      append([]string{}, d.path...),
		editing:   d.editor != nil,
		searching: d.search != nil,
	}
}

// expireNotice anchors a new notice to the view showing it, and drops a
// notice once the dialog has left that view.
func (d *SetupDialog) expireNotice() {
	if d.notice == nil {
		d.noticeAnchor = nil
		return
	}
	text := *d.notice
	view := d.noticeView()
	if d.noticeAnchor != nil && d.noticeAnchor.text == text {
		if !d.noticeAnchor.view.equal(view) {
			d.notice = nil
			d.noticeAnchor = nil
		}
		return
	}
	d.noticeAnchor = &NoticeAnchor{text: text, view: view}
}

// prepare declares the controls of the current screen on the form.
func (d *SetupDialog) prepare() {
	panic("unported: mj-tui/src/setup/form.rs prepare")
}

// Setup answers the dashboard's modal questions (ModalSurface) through
// whichever of its two forms is on top: the review-settings editor when it
// is open, the setup form otherwise.

func (d *SetupDialog) confirmationOpen() bool {
	if d.reviewEditor != nil {
		return d.reviewEditor.form.ConfirmationOpen()
	}
	return d.form.ConfirmationOpen()
}

func (d *SetupDialog) renderConfirmation(frame *chat.Frame, area uv.Rectangle, surfaces *chat.FrameSurfaces) {
	if d.reviewEditor != nil {
		d.reviewEditor.form.RenderConfirmation(frame, area, surfaces)
		return
	}
	d.form.RenderConfirmation(frame, area, surfaces)
}

func (d *SetupDialog) handlesMouse(column, row int) bool {
	if d.reviewEditor != nil {
		form := &d.reviewEditor.form
		return form.CapturesPointer() || form.Contains(column, row)
	}
	return d.form.CapturesPointer() || d.form.Contains(column, row)
}

func (d *SetupDialog) cancelPointer() bool {
	if d.reviewEditor != nil {
		form := &d.reviewEditor.form
		changed := form.CapturesPointer()
		form.CancelPointer()
		return changed
	}
	changed := d.form.CapturesPointer()
	d.form.CancelPointer()
	return changed
}

func (d *SetupDialog) resetGeometry() {
	if d.reviewEditor != nil {
		d.reviewEditor.form.ResetGeometry()
		return
	}
	d.form.ResetGeometry()
}

// textInputFocused: field ownership comes from this page's descriptor, not
// retained draft buffers.
func (d *SetupDialog) textInputFocused() bool {
	if d.form.IsFocused(SetupControlField) {
		return true
	}
	if _, ok := d.focusedFieldPath(); ok {
		return true
	}
	return d.search != nil && d.form.IsFocused(SetupControlSearch)
}

func (d *SetupDialog) prepareDialogState() {
	d.form.SetActionRole(SetupControlCancel, chat.ActionRoleCancel)
	d.form.SetActionRole(SetupControlBack, chat.ActionRoleBack)
	switch {
	case d.reviewEditor != nil:
		d.reviewEditor.prepareDialogState()
	case d.managedSetup != nil:
		d.form.SetDirty(false)
		d.form.SetDismissActions(nil)
		d.form.SetDefaultAction(SetupControlManagedContinue)
	case d.editor != nil:
		d.form.SetDirty(false)
		d.form.SetDismissActions(nil)
		d.form.SetDefaultAction(SetupControlApply)
	default:
		// The outer setup draft uses its normalized saved-config comparison.
		d.form.SetDirty(false)
		d.form.SetDismissActions(nil)
		d.form.SetDefaultAction(SetupControlSave)
	}
}

func (d *SetupDialog) layerDetail() string {
	panic("unported: mj-tui/src/setup/form.rs layer_detail")
}

func (d *SetupDialog) reconcileControls() {
	panic("unported: mj-tui/src/setup/form.rs reconcile_controls")
}

// emptyFieldHint is the placeholder an empty field shows.
func (d *SetupDialog) emptyFieldHint(path []string) (string, bool) {
	panic("unported: mj-tui/src/setup/form.rs empty_field_hint")
}

func (d *SetupDialog) choiceLabel(path []string, value any) string {
	return client.OptionLabel(path, value, d.draft)
}

func fieldInput(path []string, text string) EditorInput {
	if _, ok := client.PathKindOf(path); ok {
		return editorInputPath(chat.PathInputFromValue(text))
	}
	return editorInputText(chat.TextInputFromValue(text))
}

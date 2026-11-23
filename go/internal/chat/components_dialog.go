// Port of mj-chat/src/components/dialog.rs.

package chat

import (
	"slices"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"
)

// Shared dialog interaction, draft protection, and presentation.

// ActionRole is the meaning of an action, independent of its label and
// position.
type ActionRole int

const (
	ActionRoleCancel ActionRole = iota
	ActionRoleBack
	ActionRoleSecondary
	ActionRolePrimary
)

// DialogAction is a dialog action declared once for keyboard behavior and
// presentation.
type DialogAction[K comparable] struct {
	ID      K
	Label   string
	Role    ActionRole
	Enabled bool
}

// dialogActionRole pairs a control with its declared role.
type dialogActionRole[K comparable] struct {
	id   K
	role ActionRole
}

// dialogDismissalScope names the draft part an action dismisses.
type dialogDismissalScope[K comparable] struct {
	id   K
	part string
}

// dialogSubmit says which action Enter in a single-line field invokes.
type dialogSubmit[K comparable] struct {
	field  K
	action K
}

// draftBaseline is the saved values of one named draft part and whether the
// current values differ from them.
type draftBaseline struct {
	name    string
	values  []string
	changed bool
}

// Dialog is persistent dialog state. Domain drafts and I/O remain with the
// screen.
//
// The embedded Form is Rust's `Deref<Target = Form<K>>`: every form method
// is available on the dialog. Create one with [NewDialog].
type Dialog[K comparable] struct {
	Form[K]
	dismissActions    []K
	actionRoles       []dialogActionRole[K]
	escapeAction      *K
	dismissalScopes   []dialogDismissalScope[K]
	submit            []dialogSubmit[K]
	baselines         []draftBaseline
	dirty             bool
	dismissalEnabled  bool
	submissionPending bool
	// pendingDismissal is the dismissal waiting on the discard prompt; nil
	// when the prompt is closed.
	pendingDismissal Interaction[K]
	confirmation     Form[bool]
	// confirmationMessage is what the discard prompt says will be lost,
	// when the owner can say more than "your changes".
	confirmationMessage *string
	menu                bool
	bounds              uv.Rectangle
}

// NewDialog creates a dialog with no controls.
func NewDialog[K comparable]() Dialog[K] {
	return Dialog[K]{
		Form:             NewForm[K](),
		dismissalEnabled: true,
		confirmation:     NewForm[bool](),
	}
}

// TrackDraft compares editable values only. Call before handling edits and
// after rendering.
func (d *Dialog[K]) TrackDraft(values []string) {
	d.TrackDraftPart("draft", values)
}

// TrackDraftPart tracks lazily populated editor fields without treating
// navigation as an edit.
func (d *Dialog[K]) TrackDraftPart(name string, values []string) {
	index := slices.IndexFunc(d.baselines, func(baseline draftBaseline) bool { return baseline.name == name })
	if index >= 0 {
		d.baselines[index].changed = !slices.Equal(d.baselines[index].values, values)
	} else {
		d.baselines = append(d.baselines, draftBaseline{name: name, values: values})
	}
	d.dirty = slices.ContainsFunc(d.baselines, func(baseline draftBaseline) bool { return baseline.changed })
}

// SetEscapeAction chooses the innermost editor action for Escape and its
// close control; nil restores Escape's own dismissal.
func (d *Dialog[K]) SetEscapeAction(action *K) {
	d.escapeAction = action
}

// SetDismissalScope says which draft part dismissing through action
// discards.
func (d *Dialog[K]) SetDismissalScope(action K, part string) {
	d.dismissalScopes = slices.DeleteFunc(d.dismissalScopes, func(scope dialogDismissalScope[K]) bool { return scope.id == action })
	d.dismissalScopes = append(d.dismissalScopes, dialogDismissalScope[K]{id: action, part: part})
}

// ForgetDraftPart stops tracking a draft part.
func (d *Dialog[K]) ForgetDraftPart(part string) {
	d.baselines = slices.DeleteFunc(d.baselines, func(baseline draftBaseline) bool { return baseline.name == part })
	d.dirty = slices.ContainsFunc(d.baselines, func(baseline draftBaseline) bool { return baseline.changed })
}

// SubmissionPending prevents duplicate submission while a supervised
// operation is pending.
func (d *Dialog[K]) SubmissionPending() bool {
	return d.submissionPending
}

// SetSubmissionPending records whether a submission is in flight.
func (d *Dialog[K]) SetSubmissionPending(pending bool) {
	d.submissionPending = pending
}

// SetConfirmationMessage says in the discard prompt which changes it
// discards. Without one the prompt says only that the changes have not been
// saved.
func (d *Dialog[K]) SetConfirmationMessage(message *string) {
	d.confirmationMessage = message
}

// SetDirty overrides whether the draft has unsaved changes.
func (d *Dialog[K]) SetDirty(dirty bool) {
	d.dirty = dirty
}

// ResetDraft starts a new editor baseline after a successful save or a
// change of task.
func (d *Dialog[K]) ResetDraft() {
	d.baselines = nil
	d.dirty = false
}

// SetDismissalEnabled locks dismissal only while the owning operation
// cannot be rolled back.
func (d *Dialog[K]) SetDismissalEnabled(enabled bool) {
	d.dismissalEnabled = enabled
}

// SetDismissActions sets the actions that dismiss the dialog.
func (d *Dialog[K]) SetDismissActions(actions []K) {
	d.dismissActions = slices.Clone(actions)
}

// SetActionRole declares the role of an action.
func (d *Dialog[K]) SetActionRole(id K, role ActionRole) {
	d.actionRoles = slices.DeleteFunc(d.actionRoles, func(entry dialogActionRole[K]) bool { return entry.id == id })
	d.actionRoles = append(d.actionRoles, dialogActionRole[K]{id: id, role: role})
	if role == ActionRolePrimary {
		d.Form.SetDefaultAction(id)
	}
}

// RenderActions renders actions in the declared semantic order with the
// shared button geometry.
func (d *Dialog[K]) RenderActions(frame *Frame, area uv.Rectangle, buttons []ButtonSpec[K]) {
	d.RenderActionsAligned(frame, area, buttons, RowAlignLeft)
}

// RenderActionsAligned draws the action row packed against align, in the
// same role order.
func (d *Dialog[K]) RenderActionsAligned(frame *Frame, area uv.Rectangle, buttons []ButtonSpec[K], align RowAlign) {
	ordered := d.orderedActions(buttons)
	ButtonRowRenderAligned(frame, area, ordered, &d.Form, align)
}

// RenderActionsStacked draws the action column packed against align, in the
// same role order.
func (d *Dialog[K]) RenderActionsStacked(frame *Frame, area uv.Rectangle, buttons []ButtonSpec[K], align ColumnAlign) {
	ordered := d.orderedActions(buttons)
	ButtonColumnRenderAligned(frame, area, ordered, &d.Form, align)
}

// RenderActionsInert draws the action row without registering hitboxes.
//
// The row counterpart of [Dialog.RenderActionsStackedInert], for a page
// that stays visible behind a popup.
func (d *Dialog[K]) RenderActionsInert(frame *Frame, area uv.Rectangle, buttons []ButtonSpec[K]) {
	ButtonRowRenderInert(frame, area, actionLabels(d.orderedActions(buttons)))
}

// SplitActions splits area into the page body and the action column beside
// it.
//
// Prefer this to [ButtonColumnSplit] so the column is sized from the same
// labels the stack renders, including a pending rewrite.
func (d *Dialog[K]) SplitActions(area uv.Rectangle, buttons []ButtonSpec[K]) ColumnSplit {
	return ButtonColumnSplit(area, d.orderedActions(buttons))
}

// RenderActionsStackedInert draws the action column without registering
// hitboxes.
//
// A page that stays visible behind a popup mirrors its stack through this
// so the controls keep their places, while a click on the overlay covering
// them cannot reach a control behind it.
func (d *Dialog[K]) RenderActionsStackedInert(frame *Frame, area uv.Rectangle, buttons []ButtonSpec[K], align ColumnAlign) {
	ButtonColumnRenderInert(frame, area, actionLabels(d.orderedActions(buttons)), align)
}

// actionLabels returns the labels of buttons in order.
func actionLabels[K comparable](buttons []ButtonSpec[K]) []string {
	labels := make([]string, 0, len(buttons))
	for _, button := range buttons {
		labels = append(labels, button.Label)
	}
	return labels
}

// orderedActions applies the pending-submission rewrite and returns the
// semantic order.
//
// Every action layout reads this, so a stacked column and a row cannot
// disagree about labels or order.
func (d *Dialog[K]) orderedActions(buttons []ButtonSpec[K]) []ButtonSpec[K] {
	panic("unported: mj-chat/src/components/dialog.rs Dialog.orderedActions")
}

// SetSubmit declares which action Enter in a single-line field invokes.
func (d *Dialog[K]) SetSubmit(field K, action K) {
	d.submit = slices.DeleteFunc(d.submit, func(entry dialogSubmit[K]) bool { return entry.field == field })
	d.submit = append(d.submit, dialogSubmit[K]{field: field, action: action})
}

// SetMenu says whether the dialog is a command menu: a command menu closes
// when clicked outside its frame; forms do not.
func (d *Dialog[K]) SetMenu(menu bool) {
	d.menu = menu
}

// SetBounds records the dialog's frame for outside-click detection.
func (d *Dialog[K]) SetBounds(bounds uv.Rectangle) {
	d.bounds = bounds
}

// CancelPointer cancels captured gestures in the form and the discard
// prompt.
func (d *Dialog[K]) CancelPointer() {
	d.Form.CancelPointer()
	d.confirmation.CancelPointer()
}

// ConfirmationOpen reports whether the discard prompt is open.
func (d *Dialog[K]) ConfirmationOpen() bool {
	return d.pendingDismissal != nil
}

// RequestDismissal passes both explicit Cancel actions and Escape through
// the draft protection.
func (d *Dialog[K]) RequestDismissal(action Interaction[K]) EventResult[Interaction[K]] {
	panic("unported: mj-chat/src/components/dialog.rs Dialog.RequestDismissal")
}

// Handle routes an event to the discard prompt when it is open, otherwise
// to the form, applying the dialog's escape, submit, and dismissal rules.
func (d *Dialog[K]) Handle(event tea.Msg) EventResult[Interaction[K]] {
	panic("unported: mj-chat/src/components/dialog.rs Dialog.Handle")
}

// RenderConfirmation draws the reusable discard prompt above the owning
// dialog without losing its focus.
func (d *Dialog[K]) RenderConfirmation(frame *Frame, area uv.Rectangle, surfaces *FrameSurfaces) {
	panic("unported: mj-chat/src/components/dialog.rs Dialog.RenderConfirmation")
}

// DeclareActions declares the dialog's actions with their roles and enabled
// states.
func (d *Dialog[K]) DeclareActions(actions []DialogAction[K]) {
	d.dismissActions = d.dismissActions[:0]
	for _, action := range actions {
		d.SetActionRole(action.ID, action.Role)
		d.Form.DeclareWithEnabled(action.ID, ControlKindButton{}, action.Enabled)
		if action.Role == ActionRoleCancel {
			d.dismissActions = append(d.dismissActions, action.ID)
		}
		if action.Role == ActionRolePrimary {
			d.Form.SetDefaultAction(action.ID)
		}
	}
}

// Clone copies the dialog, cloning its forms (Rust's derived Clone).
func (d *Dialog[K]) Clone() Dialog[K] {
	panic("unported: mj-chat/src/components/dialog.rs Dialog.Clone")
}

// Equal compares two dialogs field by field (Rust's derived PartialEq).
func (d *Dialog[K]) Equal(other *Dialog[K]) bool {
	panic("unported: mj-chat/src/components/dialog.rs Dialog.Equal")
}

// DialogShell is the common modal layout that keeps actions out of the
// scrolling body.
type DialogShell struct{}

// DialogLayout is the body, status row, and action row of a dialog.
type DialogLayout struct {
	Body    uv.Rectangle
	Status  uv.Rectangle
	Actions uv.Rectangle
}

// DialogShellPaddedInner leaves one cell of padding inside the top and side
// borders, none at the bottom. Reserve a status row with
// `DialogShellLayout(inner, 1)` for the gap above actions.
func DialogShellPaddedInner(popup uv.Rectangle) uv.Rectangle {
	panic("unported: mj-chat/src/components/dialog.rs DialogShellPaddedInner")
}

// DialogShellLayout splits inner into the body, statusRows of status, and
// one action row.
func DialogShellLayout(inner uv.Rectangle, statusRows int) DialogLayout {
	height := inner.Dy()
	footer := min(height, 1)
	status := min(statusRows, max(height-footer, 0))
	bodyHeight := max(height-(footer+status), 0)
	x, y, width := inner.Min.X, inner.Min.Y, inner.Dx()
	return DialogLayout{
		Body:    uv.Rect(x, y, width, bodyHeight),
		Status:  uv.Rect(x, y+bodyHeight, width, status),
		Actions: uv.Rect(x, y+bodyHeight+status, width, footer),
	}
}

// DialogShellHints is the footer hint line for a command menu (commands) or
// a selection list.
func DialogShellHints(commands bool) Line {
	panic("unported: mj-chat/src/components/dialog.rs DialogShellHints")
}

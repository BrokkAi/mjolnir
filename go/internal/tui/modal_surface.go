// Port of mj-tui/src/modal_surface.rs.
//
// One erased view of whichever modal is open. The daemon presence overlay
// takes precedence while preserving the local mode.
//
// Every Mode variant except the dashboard carries a payload, and the
// dashboard asks each of them the same short list of questions: is a
// confirmation open, does this pointer land on you, is one of your text
// fields focused. Before this interface each question was answered by its
// own match over the mode listing every variant, so adding a modal meant
// editing seven places and forgetting one was silent.
//
// ModalSurface is that question list behind an interface. Most payloads
// answer every question through the single chat.Dialog they own, so they
// implement the smaller DialogModal instead and are wrapped by
// dialogModalSurface, which stands in for Rust's blanket implementation.
// SetupDialog answers differently and implements ModalSurface by hand; it
// lives in setup_form.go because it reads that dialog's private fields.
//
// modeSurface is now the only exhaustive per-variant list outside rendering
// and event handling, which is the point: a new variant is added there and
// nowhere else.
//
// Rust's DialogModal default methods are optional Go interfaces: a payload
// that has a textInputFocused, prepareDialogState or layerDetail method
// answers with it, and dialogModalSurface supplies Rust's default otherwise.
// The optional prepare method is named prepareDialogState, the
// ModalSurface name, because CommandPalette and ResumeDialog own unrelated
// `prepare` methods and every Rust `DialogModal::prepare` either is or calls
// the payload's prepare_dialog_state.

package tui

import (
	"fmt"

	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// ModalSurface is what the dashboard needs from the modal that is currently
// open.
type ModalSurface interface {
	confirmationOpen() bool
	renderConfirmation(frame *chat.Frame, area uv.Rectangle, surfaces *chat.FrameSurfaces)
	handlesMouse(column int, row int) bool
	// cancelPointer releases a captured pointer gesture; true if one was
	// captured.
	cancelPointer() bool
	resetGeometry()
	// textInputFocused says whether the keyboard's text keys belong to this
	// modal rather than to the dashboard's own shortcuts.
	textInputFocused() bool
	// prepareDialogState declares the roles, drafts, and default action of
	// the modal's form. Run before every event and every frame.
	prepareDialogState()
	// layerDetail is the part of the repaint layer key that distinguishes two
	// states of the same modal, such as a wizard step.
	layerDetail() string
}

// DialogModal is a modal whose whole erased behaviour is the one dialog form
// it owns. The optional methods are described in the file comment.
type DialogModal[K comparable] interface {
	dialog() *chat.Dialog[K]
}

// dialogModalTextInput is DialogModal::text_input_focused, default false.
type dialogModalTextInput interface {
	textInputFocused() bool
}

// dialogModalPrepare is DialogModal::prepare, default nothing.
type dialogModalPrepare interface {
	prepareDialogState()
}

// dialogModalLayerDetail is DialogModal::layer_detail, default empty.
type dialogModalLayerDetail interface {
	layerDetail() string
}

// dialogModalSurface is Rust's `impl<T: DialogModal> ModalSurface for T`.
type dialogModalSurface[K comparable] struct {
	modal DialogModal[K]
}

// asModalSurface wraps a DialogModal payload as the ModalSurface the
// dashboard asks.
func asModalSurface[K comparable](modal DialogModal[K]) ModalSurface {
	return dialogModalSurface[K]{modal: modal}
}

func (s dialogModalSurface[K]) confirmationOpen() bool {
	return s.modal.dialog().ConfirmationOpen()
}

func (s dialogModalSurface[K]) renderConfirmation(frame *chat.Frame, area uv.Rectangle, surfaces *chat.FrameSurfaces) {
	s.modal.dialog().RenderConfirmation(frame, area, surfaces)
}

func (s dialogModalSurface[K]) handlesMouse(column int, row int) bool {
	form := s.modal.dialog()
	return form.CapturesPointer() || form.Contains(column, row)
}

func (s dialogModalSurface[K]) cancelPointer() bool {
	form := s.modal.dialog()
	captured := form.CapturesPointer()
	form.CancelPointer()
	return captured
}

func (s dialogModalSurface[K]) resetGeometry() {
	s.modal.dialog().ResetGeometry()
}

func (s dialogModalSurface[K]) textInputFocused() bool {
	if modal, ok := s.modal.(dialogModalTextInput); ok {
		return modal.textInputFocused()
	}
	return false
}

func (s dialogModalSurface[K]) prepareDialogState() {
	if modal, ok := s.modal.(dialogModalPrepare); ok {
		modal.prepareDialogState()
	}
}

func (s dialogModalSurface[K]) layerDetail() string {
	if modal, ok := s.modal.(dialogModalLayerDetail); ok {
		return modal.layerDetail()
	}
	return ""
}

// DialogModal for WelcomeDialog.

func (d *WelcomeDialog) dialog() *chat.Dialog[WelcomeAction] { return &d.form }

func (d *WelcomeDialog) prepareDialogState() {
	d.form.SetDismissActions([]WelcomeAction{WelcomeActionContinue})
	d.form.SetDefaultAction(WelcomeActionContinue)
}

// DialogModal for NoticeLogDialog.

func (d *NoticeLogDialog) dialog() *chat.Dialog[DialogControl] { return &d.form }

func (d *NoticeLogDialog) prepareDialogState() {
	d.form.SetDismissActions([]DialogControl{DialogControlNoticeLogClose})
	d.form.SetDefaultAction(DialogControlNoticeLogClose)
}

// DialogModal for SessionCpuReportDialog.

func (d *SessionCpuReportDialog) dialog() *chat.Dialog[DialogControl] { return &d.form }

func (d *SessionCpuReportDialog) prepareDialogState() {
	d.form.SetDismissActions([]DialogControl{DialogControlSessionCpuReportClose})
	d.form.SetDefaultAction(DialogControlSessionCpuReportClose)
}

// DialogModal for SessionUsageDialog.

func (d *SessionUsageDialog) dialog() *chat.Dialog[UsageControl] { return &d.form }

func (d *SessionUsageDialog) prepareDialogState() {
	d.form.SetDismissActions([]UsageControl{UsageControlClose})
	d.form.SetDefaultAction(UsageControlClose)
}

// DialogModal for ChangedFilesDialog.

func (d *ChangedFilesDialog) dialog() *chat.Dialog[DialogControl] { return &d.form }

func (d *ChangedFilesDialog) prepareDialogState() {
	d.form.SetDismissActions([]DialogControl{DialogControlChangedFilesClose})
	d.form.SetDefaultAction(DialogControlChangedFilesClose)
}

// DialogModal for RenameEditor.

func (e *RenameEditor) dialog() *chat.Dialog[DialogControl] { return &e.form }

func (e *RenameEditor) textInputFocused() bool {
	return e.form.IsFocused(DialogControlField)
}

func (e *RenameEditor) prepareDialogState() {
	e.form.TrackDraft([]string{e.title.String()})
	e.form.SetDismissActions([]DialogControl{DialogControlCancel})
	e.form.SetSubmit(DialogControlField, DialogControlSave)
	e.form.SetDefaultAction(DialogControlSave)
}

// DialogModal for GitIdentityDialog: textInputFocused and prepareDialogState
// are the dialog's own methods in dialogs_git_identity.go.

func (d *GitIdentityDialog) dialog() *chat.Dialog[GitIdentityControl] { return &d.form }

// DialogModal for ConfigIdEditor.

func (e *ConfigIdEditor) dialog() *chat.Dialog[DialogControl] { return &e.form }

func (e *ConfigIdEditor) textInputFocused() bool {
	return e.form.IsFocused(DialogControlField)
}

func (e *ConfigIdEditor) prepareDialogState() {
	e.form.TrackDraft([]string{e.value.String()})
	e.form.SetDismissActions([]DialogControl{DialogControlCancel})
	e.form.SetSubmit(DialogControlField, DialogControlSave)
	e.form.SetDefaultAction(DialogControlSave)
}

// DialogModal for RepositoryOriginDialog.

func (d *RepositoryOriginDialog) dialog() *chat.Dialog[DialogControl] { return &d.form }

func (d *RepositoryOriginDialog) textInputFocused() bool {
	return d.form.IsFocused(DialogControlField)
}

func (d *RepositoryOriginDialog) prepareDialogState() {
	d.form.TrackDraft([]string{d.replacement.String()})
	d.form.SetDismissActions([]DialogControl{DialogControlCancel})
	d.form.SetSubmit(DialogControlField, DialogControlPrimary)
	d.form.SetDefaultAction(DialogControlPrimary)
}

// DialogModal for the dialogs that take every default.

func (d *TargetActionsDialog) dialog() *chat.Dialog[DialogControl]      { return &d.form }
func (d *WebDialog) dialog() *chat.Dialog[DialogControl]                { return &d.form }
func (p *ImportProgress) dialog() *chat.Dialog[DialogControl]           { return &p.form }
func (c *ImportBundleConfirmation) dialog() *chat.Dialog[DialogControl] { return &c.form }
func (d *ConfirmDialog) dialog() *chat.Dialog[DialogControl]            { return &d.form }

// DialogModal for CommandPalette.

func (p *CommandPalette) dialog() *chat.Dialog[PaletteControl] { return &p.form }

// textInputFocused: the palette's query is a text field, so Ctrl-C closes
// the palette and a paste lands in the query rather than on the dashboard.
func (p *CommandPalette) textInputFocused() bool {
	return p.form.IsFocused(PaletteControlQuery)
}

// DialogModal for ResumeDialog.

func (d *ResumeDialog) dialog() *chat.Dialog[ResumeFocus] { return &d.form }

func (d *ResumeDialog) textInputFocused() bool {
	return d.form.IsFocused(ResumeFocusSearch)
}

// DialogModal for WorkspaceManager: prepareDialogState is the manager's own
// method in workspaces.go.

func (m *WorkspaceManager) dialog() *chat.Dialog[WorkspaceControl] { return &m.form }

func (m *WorkspaceManager) textInputFocused() bool {
	return m.form.IsFocused(WorkspaceControlName)
}

func (m *WorkspaceManager) layerDetail() string {
	return fmt.Sprintf("%#v", m.view)
}

// DialogModal for ContainerEditor: prepareDialogState is the editor's own
// method in dialogs_container.go.

func (e *ContainerEditor) dialog() *chat.Dialog[ContainerEditFocus] { return &e.form }

// textInputFocused is deliberately `field() != nil` rather than a focus
// test: when nothing is focused the editor behaves as though the first field
// were, and the dashboard's text routing has always followed that.
func (e *ContainerEditor) textInputFocused() bool {
	return e.field() != nil
}

// DialogModal for NewWizard: prepareDialogState is the wizard's own method
// in wizards.go.

func (w *NewWizard) dialog() *chat.Dialog[WizardControl] { return &w.form }

func (w *NewWizard) textInputFocused() bool {
	panic("unported: mj-tui/src/modal_surface.rs NewWizard::text_input_focused")
}

func (w *NewWizard) layerDetail() string {
	return fmt.Sprintf("%#v", w.step)
}

// DialogModal for ResumeWizard: prepareDialogState is the wizard's own
// method in wizards.go.

func (w *ResumeWizard) dialog() *chat.Dialog[WizardControl] { return &w.form }

func (w *ResumeWizard) textInputFocused() bool {
	if id, ok := w.form.Focused(); ok {
		switch w.step {
		case WizardStepTarget:
			return id == WizardControlResourceCpu || id == WizardControlResourceMemory
		case WizardStepMounts:
			return id == WizardControlMountSource || id == WizardControlMountDestination
		default:
			return false
		}
	}
	// See the note on the new-session wizard.
	return w.step == WizardStepMounts
}

func (w *ResumeWizard) layerDetail() string {
	return fmt.Sprintf("%#v", w.step)
}

// modeSurface is Mode::surface and Mode::surface_mut: the modal mode carries,
// nil for the dashboard. The surface refers to the payload inside *mode, so
// changes it makes stay in the mode. Every Mode variant that carries a modal
// is listed here, so a new variant is added here and nowhere else.
func modeSurface(mode *Mode) ModalSurface {
	panic("unported: mj-tui/src/modal_surface.rs Mode::surface")
}

// activeModal is the modal the dashboard asks: the operator-away overlay
// while it is up, else the open mode's modal; nil over the bare dashboard.
func (d *DashboardState) activeModal() ModalSurface {
	panic("unported: mj-tui/src/modal_surface.rs DashboardState::active_modal")
}

// activeModalMut is activeModal for changes. Go's ModalSurface already
// refers to the payload, so it answers as activeModal does.
func (d *DashboardState) activeModalMut() ModalSurface {
	return d.activeModal()
}

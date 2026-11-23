// Port of mj-tui/src/component_events.rs.
//
// Routes terminal input to the active reusable form before background panes.

package tui

import (
	"fmt"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
)

// RouteQuestionPress lets a press on a question's answer rows still reach
// the form, which acts on release, while the selection engine tracks it too
// so a drag can select the drawn text instead. It returns the event to hand
// to the form, or nil when the caller routes it as usual.
func (d *DashboardState) RouteQuestionPress(selection *chat.SelectionState, chatOwnsPointer bool, event tea.Msg) tea.Msg {
	switch mouse := event.(type) {
	case tea.MouseClickMsg:
		if mouse.Button != tea.MouseLeft {
			return nil
		}
		d.questionPress = nil
		surfaces := d.FrameSurfaces()
		answerRow := false
		if !d.ModalOpen() {
			if surface := surfaces.SurfaceAt(mouse.X, mouse.Y); surface != nil {
				switch surface.ID.(type) {
				// A boolean question's title and description lie inside its
				// checkbox hitbox.
				case chat.SurfaceIdNotificationFormBody, chat.SurfaceIdNotificationText, chat.SurfaceIdElicitationMessage:
					answerRow = true
				case chat.SurfaceIdModalBody:
					answerRow = surfaces.Surface(chat.SurfaceIdElicitationMessage{}) != nil
				}
			}
		}
		if !(chatOwnsPointer && answerRow) {
			return nil
		}
		selection.OnMouseDown(mouse.X, mouse.Y, d.FrameSurfaces())
		if selection.OwnsPointer() {
			press := uv.Pos(mouse.X, mouse.Y)
			d.questionPress = &press
		}
		return event
	case tea.MouseReleaseMsg:
		// A release in the press cell is the form's click. One anywhere else
		// goes to the selection engine, which tracks its position even when
		// the terminal sent no drag event.
		if mouse.Button != tea.MouseLeft || d.questionPress == nil ||
			*d.questionPress != uv.Pos(mouse.X, mouse.Y) {
			return nil
		}
		if _, selecting := selection.Range(); selecting {
			return nil
		}
		d.questionPress = nil
		selection.Clear()
		return event
	}
	return nil
}

// SettleQuestionPress releases the form's armed control once the press
// became a text selection, or the gesture ended without reaching the form:
// the form gets no click then.
func (d *DashboardState) SettleQuestionPress(selection *chat.SelectionState, chats map[string]*chat.ActiveChat) {
	if d.questionPress == nil {
		return
	}
	if _, selecting := selection.Range(); selecting || !selection.OwnsPointer() {
		d.questionPress = nil
		d.CancelAllComponentPointers(chats)
	}
}

// CancelAllComponentPointers disarms every chat presentation, including warm
// forms whose session differs from the focused conversation, as global
// commands require.
func (d *DashboardState) CancelAllComponentPointers(chats map[string]*chat.ActiveChat) {
	d.CancelComponentPointer()
	for _, active := range chats {
		active.CancelComponentPointer()
	}
}

// PrepareComponentEvent broadcasts focus loss to every presentation,
// including warm cards outside the focused conversation.
func (d *DashboardState) PrepareComponentEvent(event tea.Msg, chats map[string]*chat.ActiveChat) {
	if _, blurred := event.(tea.BlurMsg); blurred {
		d.CancelAllComponentPointers(chats)
	}
}

func (d *DashboardState) dialogLayerKey() string {
	detail := ""
	if modal := d.activeModal(); modal != nil {
		detail = modal.layerDetail()
	}
	// The mode's variant stands in for Rust's mem::discriminant: the key is
	// only ever compared with an earlier key.
	return fmt.Sprintf("%t/%T/%s/%t", d.awayDialog() != nil, d.mode, detail, d.dialogConfirmationOpen())
}

func (d *DashboardState) prepareDialogState() {
	d.showPendingWelcome()
	if modal := d.activeModalMut(); modal != nil {
		modal.prepareDialogState()
	}
}

func (d *DashboardState) dialogConfirmationOpen() bool {
	modal := d.activeModal()
	return modal != nil && modal.confirmationOpen()
}

func (d *DashboardState) renderDialogConfirmation(frame *chat.Frame, area uv.Rectangle, surfaces *chat.FrameSurfaces) {
	if modal := d.activeModal(); modal != nil {
		modal.renderConfirmation(frame, area, surfaces)
	}
}

// componentModalOpen reports whether the open mode routes events away from
// the dashboard.
func (d *DashboardState) componentModalOpen() bool {
	_, dashboard := d.mode.(ModeDashboard)
	return d.awayDialog() != nil || !dashboard
}

// ComponentPointerCaptured reports whether an already pressed dashboard
// control owns its drag and release.
func (d *DashboardState) ComponentPointerCaptured() bool {
	return d.surfaceForm.CapturesPointer() || d.workspaceScrollbar.IsDragging()
}

// ComponentHandlesMouse reports whether a form owns this pointer event, ahead
// of selectable body text.
func (d *DashboardState) ComponentHandlesMouse(mouse tea.MouseMsg) bool {
	if d.awayDialog() != nil {
		return true
	}
	if d.resumeScrollbarHandlesMouse(mouse) || d.workspaceScrollbar.IsDragging() {
		return true
	}
	_, palette := d.mode.(ModePalette)
	click, isClick := mouse.(tea.MouseClickMsg)
	if d.paneMenu != nil || (isClick && click.Button == tea.MouseRight) ||
		d.dialogConfirmationOpen() || palette {
		return true
	}
	position := mouse.Mouse()
	if _, dashboard := d.mode.(ModeDashboard); dashboard {
		return d.surfaceForm.CapturesPointer() ||
			d.surfaceForm.Contains(position.X, position.Y) ||
			(d.workspacePaneArea != nil && uv.Pos(position.X, position.Y).In(*d.workspacePaneArea))
	}
	modal := d.activeModal()
	return modal != nil && modal.handlesMouse(position.X, position.Y)
}

// CancelComponentPointer releases any pressed dashboard control. Global
// shortcuts use this when they take ownership of a pointer gesture; clearing
// an armed control is itself a visible change and therefore requests a
// repaint.
func (d *DashboardState) CancelComponentPointer() bool {
	workspaceDragging := d.workspaceScrollbar.IsDragging()
	if workspaceDragging {
		d.workspaceScrollbar.Clear()
	}
	changed := d.surfaceForm.CapturesPointer()
	d.surfaceForm.CancelPointer()
	changed = changed || workspaceDragging
	if modal := d.activeModalMut(); modal != nil {
		changed = modal.cancelPointer() || changed
	}
	return changed
}

func (d *DashboardState) resetComponentGeometry() {
	d.quotaRowAreas = d.quotaRowAreas[:0]
	d.quotaPoolAreas = d.quotaPoolAreas[:0]
	d.notifications.formArea = nil
	d.surfaceForm.ResetGeometry()
	if modal := d.activeModalMut(); modal != nil {
		modal.resetGeometry()
	}
}

func (d *DashboardState) handleComponentEvent(event tea.Msg) DashboardAction {
	if d.awayDialog() != nil {
		return d.handleAwayEvent(event)
	}
	d.prepareDialogState()
	switch d.mode.(type) {
	case ModePalette:
		return d.handlePaletteEvent(event)
	case ModeResumeDialog:
		return d.handleResumeDialogEvent(event)
	case ModeWorkspaceManager:
		return d.handleWorkspaceManagerEvent(event)
	}
	mode := d.mode
	d.mode = ModeDashboard{}
	switch mode := mode.(type) {
	case ModeEditContainer:
		return d.handleContainerEditEvent(event, *mode.ContainerEditor)
	case ModeSetup:
		return d.handleSetupEvent(event, mode.SetupDialog)
	case ModeWelcome:
		return d.handleWelcomeEvent(event, mode.WelcomeDialog)
	case ModeRename:
		return d.handleRenameEvent(event, *mode.RenameEditor)
	case ModeGitIdentity:
		return d.handleGitIdentityEvent(event, *mode.GitIdentityDialog)
	case ModeChangedFiles:
		return d.handleChangedFilesEvent(event, *mode.ChangedFilesDialog)
	case ModeSessionCpuReport:
		return d.handleSessionCpuReportEvent(event, *mode.SessionCpuReportDialog)
	case ModeSessionUsageReport:
		return d.handleSessionUsageReportEvent(event, *mode.SessionUsageDialog)
	case ModeNoticeLog:
		return d.handleNoticeLogEvent(event, *mode.NoticeLogDialog)
	case ModeConfigId:
		return d.handleConfigIDEvent(event, *mode.ConfigIdEditor)
	case ModeRepositoryOrigin:
		return d.handleRepositoryOriginEvent(event, *mode.RepositoryOriginDialog)
	case ModeTargetActions:
		return d.handleTargetActionsEvent(event, *mode.TargetActionsDialog)
	case ModeWeb:
		return d.handleWebEvent(event, *mode.WebDialog)
	case ModeImporting:
		return d.handleImportProgressEvent(event, *mode.ImportProgress)
	case ModeConfirmImportBundle:
		return d.handleImportBundleEvent(event, *mode.ImportBundleConfirmation)
	case ModeConfirm:
		return d.handleConfirmationEvent(event, *mode.ConfirmDialog)
	case ModeNew:
		return d.handleWizardEvent(event, mode.NewWizard)
	case ModeResume:
		return d.handleWizardEvent(event, mode.ResumeWizard)
	default:
		d.mode = mode
		return DashboardActionNone{}
	}
}

// Port of mj-tui/src/dialogs/git_identity.rs.
//
// Asks for the host's Git author identity before the first new session of a
// run, when Git on this host has none. Sessions inherit the host's global
// identity, so without it no session can commit.
//
// The dashboard never touches Git itself: it returns
// DashboardActionCheckGitIdentity and DashboardActionSaveGitIdentity for the
// binary to ask the daemon in the background, since the daemon's Git
// environment is the one sessions inherit, and takes the results through
// DashboardState.ApplyGitIdentityCheck and
// DashboardState.ApplyGitIdentitySave.

package tui

import (
	"fmt"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
)

// gitIdentityExplanation is Rust's EXPLANATION, renamed for operator_away's
// EXPLANATION in the same package.
const gitIdentityExplanation = "Git on this host has no commit author, so sessions cannot commit. " +
	"Mjolnir saves this to your global Git configuration."

const gitIdentityUnsetNotice = "Git on this host has no commit author, so sessions cannot commit; start a new session to set one."

const gitIdentitySkippedNotice = "Sessions cannot commit until Git has an identity; Mjolnir will ask again next start."

// GitIdentityGateKind is where a GitIdentityGate stands.
type GitIdentityGateKind uint8

const (
	// GitIdentityGateSettled: no check is wanted, or this run already
	// decided. Embedders that never arm the gate, tests among them, start
	// here. It is the zero value, as it is Rust's Default.
	GitIdentityGateSettled GitIdentityGateKind = iota
	// GitIdentityGateUnchecked: the next new session asks for a check first.
	GitIdentityGateUnchecked
	// GitIdentityGateChecking: the check is running and holds the request
	// that started it in Resume.
	GitIdentityGateChecking
)

// GitIdentityGate is where this run stands on the host's Git identity. The
// dialog being open is not a state here: it is the GitIdentity mode, so
// closing it by any route can never leave the gate waiting on a dialog that
// is gone.
//
// Rust's enum with one data-carrying variant is a kind and the held request,
// so the zero value is Settled.
type GitIdentityGate struct {
	Kind GitIdentityGateKind
	// Resume is the request the check holds; set only while Kind is
	// GitIdentityGateChecking.
	Resume GitIdentityResume
}

// GitIdentityResume is the new-session request a check or the dialog holds,
// continued once the identity is settled.
type GitIdentityResume interface {
	isGitIdentityResume()
}

// GitIdentityResumeBeginNew opens the new-session flow, as the new-session
// key does.
type GitIdentityResumeBeginNew struct{}

// GitIdentityResumeLaunch runs a launch that was already decided, such as a
// fast-start recipe.
type GitIdentityResumeLaunch struct {
	Launch DashboardAction
}

func (GitIdentityResumeBeginNew) isGitIdentityResume() {}
func (GitIdentityResumeLaunch) isGitIdentityResume()   {}

// GitIdentityControl is a control of the Git identity dialog.
type GitIdentityControl int

const (
	GitIdentityControlName GitIdentityControl = iota
	GitIdentityControlEmail
	GitIdentityControlSkip
	GitIdentityControlSave
)

// GitIdentityDialog asks for the host's missing Git author identity.
type GitIdentityDialog struct {
	resume GitIdentityResume
	name   chat.TextInput
	email  chat.TextInput
	// saving: a save is running; input waits for its result.
	saving bool
	error  *string
	form   chat.Dialog[GitIdentityControl]
}

// newGitIdentityDialog is GitIdentityDialog::new; name and email are the
// parts Git already has, nil when missing.
func newGitIdentityDialog(resume GitIdentityResume, name *string, email *string) GitIdentityDialog {
	initial := GitIdentityControlName
	if name != nil {
		initial = GitIdentityControlEmail
	}
	form := chat.NewDialog[GitIdentityControl]()
	form.Declare(GitIdentityControlName, chat.ControlKindTextField{})
	form.Declare(GitIdentityControlEmail, chat.ControlKindTextField{})
	form.Declare(GitIdentityControlSkip, chat.ControlKindButton{})
	form.Declare(GitIdentityControlSave, chat.ControlKindButton{})
	form.EndFrame(initial)
	var nameValue, emailValue string
	if name != nil {
		nameValue = *name
	}
	if email != nil {
		emailValue = *email
	}
	return GitIdentityDialog{
		resume: resume,
		name:   chat.TextInputFromValue(nameValue),
		email:  chat.TextInputFromValue(emailValue),
		form:   form,
	}
}

// Clone is Rust's derived Clone: the form is copied, not shared.
func (d *GitIdentityDialog) Clone() GitIdentityDialog {
	clone := *d
	clone.form = d.form.Clone()
	return clone
}

func (d *GitIdentityDialog) textInputFocused() bool {
	if d.saving {
		return false
	}
	id, ok := d.form.Focused()
	return !ok || id == GitIdentityControlName || id == GitIdentityControlEmail
}

func (d *GitIdentityDialog) prepareDialogState() {
	values := []string{d.name.String(), d.email.String()}
	form := &d.form
	form.TrackDraft(values)
	form.SetDismissActions([]GitIdentityControl{GitIdentityControlSkip})
	form.SetSubmit(GitIdentityControlName, GitIdentityControlSave)
	form.SetSubmit(GitIdentityControlEmail, GitIdentityControlSave)
	form.SetDefaultAction(GitIdentityControlSave)
}

// RequireGitIdentityCheck asks for a Git identity check before this run's
// first new session. The binary arms this; a dashboard that is never armed
// skips the check.
func (d *DashboardState) RequireGitIdentityCheck() {
	d.gitIdentityGate = GitIdentityGate{Kind: GitIdentityGateUnchecked}
}

// gitIdentityGateAction is what the new-session key does first, or nil once
// this run has settled the identity and the request can proceed.
func (d *DashboardState) gitIdentityGateAction() DashboardAction {
	return d.holdForGitIdentity(GitIdentityResumeBeginNew{})
}

// GateLaunchOnGitIdentity is the path every launch that bypasses the
// new-session key, such as a fast-start recipe, passes through: it runs now
// once the identity is settled, and otherwise waits for the check.
func (d *DashboardState) GateLaunchOnGitIdentity(launch DashboardAction) DashboardAction {
	if held := d.holdForGitIdentity(GitIdentityResumeLaunch{Launch: launch}); held != nil {
		return held
	}
	return launch
}

// holdForGitIdentity returns the action that holds the request for a check,
// or nil when the identity is settled.
func (d *DashboardState) holdForGitIdentity(resume GitIdentityResume) DashboardAction {
	switch d.gitIdentityGate.Kind {
	case GitIdentityGateUnchecked:
		d.gitIdentityGate = GitIdentityGate{Kind: GitIdentityGateChecking, Resume: resume}
		d.SetNotice("Checking the Git identity on this host…")
		return DashboardActionCheckGitIdentity{}
	case GitIdentityGateChecking:
		d.SetNotice("Still checking the Git identity on this host…")
		return DashboardActionNone{}
	default:
		return nil
	}
}

// ApplyGitIdentityCheck takes the identity the daemon read, or the error
// that stopped the read: it continues the held request when no setup is
// needed, and otherwise asks for the missing part. A check that failed is
// reported and does not hold the session back.
func (d *DashboardState) ApplyGitIdentityCheck(identity client.GitIdentity, err error) {
	// Rust takes the gate (leaving it Settled) before looking at it.
	gate := d.gitIdentityGate
	d.gitIdentityGate = GitIdentityGate{}
	if gate.Kind != GitIdentityGateChecking {
		return
	}
	resume := gate.Resume
	switch {
	case err == nil && !identity.NeedsSetup():
		d.ClearNotice()
		d.settleGitIdentity(resume)
	case err == nil:
		if _, dashboard := d.mode.(ModeDashboard); dashboard {
			// The dialog settles the gate; until then, the next new session
			// checks again.
			d.gitIdentityGate = GitIdentityGate{Kind: GitIdentityGateUnchecked}
			d.ClearNotice()
			dialog := newGitIdentityDialog(resume, identity.Name, identity.Email)
			d.mode = ModeGitIdentity{&dialog}
		} else {
			// The user moved on while Git was read; the dialog does not open
			// over their work, and the request still runs.
			d.gitIdentityGate = GitIdentityGate{Kind: GitIdentityGateUnchecked}
			d.SetNotice(gitIdentityUnsetNotice)
			d.resumeAfterGitIdentity(resume)
		}
	default:
		d.SetFailureNotice(fmt.Sprintf("Could not read the Git identity on this host: %s", err))
		d.settleGitIdentity(resume)
	}
}

// ApplyGitIdentitySave takes the result of saving the identity. A failure
// keeps the dialog and what was typed, with the reason under the fields.
func (d *DashboardState) ApplyGitIdentitySave(identity client.GitIdentity, err error) {
	open, isOpen := d.mode.(ModeGitIdentity)
	saving := isOpen && open.saving
	if err == nil {
		var name, email string
		if identity.Name != nil {
			name = *identity.Name
		}
		if identity.Email != nil {
			email = *identity.Email
		}
		d.SetNotice(fmt.Sprintf("Git identity saved: %s <%s>", name, email))
		if saving {
			d.mode = ModeDashboard{}
			d.settleGitIdentity(open.resume)
		} else {
			d.gitIdentityGate = GitIdentityGate{Kind: GitIdentityGateSettled}
		}
		return
	}
	if saving {
		open.saving = false
		message := err.Error()
		open.error = &message
		return
	}
	d.SetFailureNotice(fmt.Sprintf("Could not save the Git identity: %s", err))
}

// TakeGitIdentityFollowup is the new-session action a settled identity check
// let through, for the binary to run as though the user had just asked for
// it; nil when there is none.
func (d *DashboardState) TakeGitIdentityFollowup() DashboardAction {
	followup := d.gitIdentityFollowup
	d.gitIdentityFollowup = nil
	return followup
}

func (d *DashboardState) settleGitIdentity(resume GitIdentityResume) {
	d.gitIdentityGate = GitIdentityGate{Kind: GitIdentityGateSettled}
	d.resumeAfterGitIdentity(resume)
}

func (d *DashboardState) resumeAfterGitIdentity(resume GitIdentityResume) {
	var action DashboardAction
	switch resume := resume.(type) {
	case GitIdentityResumeBeginNew:
		// The new-session flow opens over the dashboard only; a user who
		// opened something else meanwhile is told how to continue.
		if _, dashboard := d.mode.(ModeDashboard); !dashboard {
			if d.gitIdentityGate.Kind == GitIdentityGateSettled {
				d.SetNotice("Git identity checked; start the new session again.")
			}
			return
		}
		action = d.beginNew()
	case GitIdentityResumeLaunch:
		action = resume.Launch
	}
	if _, none := action.(DashboardActionNone); action != nil && !none {
		d.gitIdentityFollowup = action
	}
}

func (d *DashboardState) handleGitIdentityEvent(event tea.Msg, dialog GitIdentityDialog) DashboardAction {
	if dialog.saving {
		d.lastEventConsumed = true
		d.mode = ModeGitIdentity{&dialog}
		return DashboardActionNone{}
	}
	result := dialog.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	skip := false
	switch interaction := result.Action.(type) {
	case chat.InteractionCancel[GitIdentityControl]:
		skip = true
	case chat.InteractionEdit[GitIdentityControl]:
		var field *chat.TextInput
		switch interaction.ID {
		case GitIdentityControlName:
			field = &dialog.name
		case GitIdentityControlEmail:
			field = &dialog.email
		}
		if field != nil {
			chat.TextFieldApply(field, interaction.Edit)
		}
		d.mode = ModeGitIdentity{&dialog}
		return DashboardActionNone{}
	case chat.InteractionActivate[GitIdentityControl]:
		if interaction.ID == GitIdentityControlSkip {
			skip = true
			break
		}
		dialog.saving = true
		dialog.error = nil
		action := DashboardActionSaveGitIdentity{Name: dialog.name.String(), Email: dialog.email.String()}
		d.mode = ModeGitIdentity{&dialog}
		return action
	}
	if skip {
		d.mode = ModeDashboard{}
		d.SetNotice(gitIdentitySkippedNotice)
		d.settleGitIdentity(dialog.resume)
		return DashboardActionNone{}
	}
	d.mode = ModeGitIdentity{&dialog}
	return DashboardActionNone{}
}

func renderGitIdentityDialog(frame *chat.Frame, area uv.Rectangle, dialog *GitIdentityDialog, surfaces *chat.FrameSurfaces) {
	popup := centeredModal(frame, surfaces, 60, 12, area)
	inner := chat.RectInner(popup, chat.Margin{Horizontal: 1, Vertical: 1})
	form := &dialog.form
	form.BeginFrame()
	title := dismissibleModalTitle(&form.Form, popup, "Set up Git identity", chat.Title(true), !dialog.saving)
	frame.RenderWidget(chat.Modal().WithTitle(title), popup)
	row := func(offset, height int) uv.Rectangle {
		return uv.Rect(inner.Min.X, inner.Min.Y+offset, inner.Dx(), min(height, max(inner.Dy()-offset, 0)))
	}
	frame.RenderWidget(
		chat.NewParagraph(chat.RawText(gitIdentityExplanation)).
			WithStyle(chat.Style{}.Foreground(chat.ThemePalette().Muted)).
			WithWrap(chat.Wrap{Trim: true}),
		row(0, 3),
	)
	for _, field := range []struct {
		offset int
		label  string
		input  *chat.TextInput
		id     GitIdentityControl
	}{
		{4, "Name:  ", &dialog.name, GitIdentityControlName},
		{5, "Email: ", &dialog.email, GitIdentityControlEmail},
	} {
		rect := row(field.offset, 1)
		labelWidth := min(len(field.label), rect.Dx())
		frame.RenderWidget(chat.RawLine(field.label), uv.Rect(rect.Min.X, rect.Min.Y, labelWidth, rect.Dy()))
		chat.TextFieldRender(
			frame,
			uv.Rect(rect.Min.X+labelWidth, rect.Min.Y, rect.Dx()-labelWidth, rect.Dy()),
			field.input,
			&form.Form,
			field.id,
		)
	}
	var status *chat.Line
	if dialog.saving {
		line := chat.StyledLine("Saving Git identity…", chat.Muted())
		status = &line
	} else if dialog.error != nil {
		line := chat.StyledLine(*dialog.error, chat.Style{}.Foreground(chat.ThemePalette().Warning))
		status = &line
	}
	if status != nil {
		frame.RenderWidget(*status, row(7, 1))
	}
	footerHeight := 0
	if inner.Dy() > 0 {
		footerHeight = 1
	}
	footer := uv.Rect(inner.Min.X, max(inner.Max.Y-1, 0), inner.Dx(), footerHeight)
	form.RenderActions(frame, footer, []chat.ButtonSpec[GitIdentityControl]{
		{ID: GitIdentityControlSkip, Label: "Skip", Enabled: !dialog.saving},
		{ID: GitIdentityControlSave, Label: "Save", Enabled: !dialog.saving},
	})
	form.EndFrame(GitIdentityControlName)
}

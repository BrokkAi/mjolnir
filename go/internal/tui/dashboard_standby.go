// Port of mj-tui/src/dashboard_standby.rs.

package tui

import (
	"maps"
	"slices"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// SetCurrentSession records the conversation the focused pane shows, which
// decides which project the compact Sessions list belongs to. Nil empties the
// pane.
func (d *DashboardState) SetCurrentSession(sessionID *string) {
	d.SetPaneSession(d.navigation.layout().Focused(), sessionID)
}

// SetPaneSession assigns the conversation a pane should show before
// background loading. Background completion never changes this assignment.
func (d *DashboardState) SetPaneSession(pane PaneId, sessionID *string) {
	if !d.navigation.assign(pane, sessionID) {
		return
	}
	d.reconcilePins()
	d.markLayoutModified()
	d.clampSelections()
}

// SessionActivityAtMs is when this session's materialized projection last
// changed, in milliseconds since the epoch. ok is false while nothing has
// been projected for it yet.
func (d *DashboardState) SessionActivityAtMs(sessionID string) (at uint64, ok bool) {
	panic("unported: mj-tui/src/dashboard_standby.rs DashboardState::session_activity_at_ms")
}

// CurrentSessionID is the session the focused pane shows, if it is not
// empty.
func (d *DashboardState) CurrentSessionID() (string, bool) {
	return d.navigation.selected()
}

// SetOpeningSession records the session an attach is running for, or clears
// it (nil) when the attach settles.
func (d *DashboardState) SetOpeningSession(sessionID *string) {
	if sessionID == nil {
		d.openingSession = nil
		return
	}
	session := *sessionID
	d.openingSession = &session
}

// OpeningSession is the session an attach is still running for, if any.
func (d *DashboardState) OpeningSession() (string, bool) {
	if d.openingSession == nil {
		return "", false
	}
	return *d.openingSession, true
}

// SelectedSessionID is the session the Sessions pane has selected. The
// conversation on screen follows this, so moving the selection moves the
// transcript.
func (d *DashboardState) SelectedSessionID() (string, bool) {
	return d.CurrentSessionID()
}

// ChatInputAvailable reports that no lifecycle transition owns the session:
// lifecycle ownership excludes input on every live chat presentation.
func (d *DashboardState) ChatInputAvailable(sessionID string) bool {
	return d.TransitionKind(sessionID) == nil && d.TransitionFailureKind(sessionID) == nil
}

func (d *DashboardState) SetSessionVerdicts(verdicts core.SnapshotMap[string, client.SessionVerdict]) {
	panic("unported: mj-tui/src/dashboard_standby.rs DashboardState::set_session_verdicts")
}

// setSessionVerdict is a test helper (Rust #[cfg(test)]).
func (d *DashboardState) setSessionVerdict(sessionID string, verdict client.SessionVerdict) {
	panic("unported: mj-tui/src/dashboard_standby.rs DashboardState::set_session_verdict")
}

func (d *DashboardState) verdict(sessionID string) client.SessionVerdict {
	panic("unported: mj-tui/src/dashboard_standby.rs DashboardState::verdict")
}

func (d *DashboardState) pendingLocalSessionOperation(sessionID string) (SessionOperationKind, bool) {
	panic("unported: mj-tui/src/dashboard_standby.rs DashboardState::pending_local_session_operation")
}

// TransitionKind is the session's lifecycle transition, nil when none. A
// terminal request may be awaiting acknowledgement; its local progress
// presentation fills the gap until the daemon publishes operation ownership.
func (d *DashboardState) TransitionKind(sessionID string) *core.SessionTransitionKind {
	if transition := d.verdict(sessionID).Transition; transition != nil {
		return transition
	}
	if operation, ok := d.pendingLocalSessionOperation(sessionID); ok {
		return operation.TransitionKind()
	}
	return nil
}

// SessionFailed reports whether the session's target failed, so there is no
// worker to attach to. Opening one is a decision (read the transcript, or
// recover it), which is why the automatic startup pick and the selection
// follow leave it alone and the band explains instead.
func (d *DashboardState) SessionFailed(sessionID string) bool {
	panic("unported: mj-tui/src/dashboard_standby.rs DashboardState::session_failed")
}

// sessionFailureText is what to show for a failed session: its recorded
// error, or a plain sentence when the daemon recorded none.
func (d *DashboardState) sessionFailureText(sessionID string) string {
	panic("unported: mj-tui/src/dashboard_standby.rs DashboardState::session_failure_text")
}

func (d *DashboardState) stoppedNotificationRow(sessionID string) bool {
	panic("unported: mj-tui/src/dashboard_standby.rs DashboardState::stopped_notification_row")
}

// stoppedFailureGuidance is what a person can do with a stopped row: resume
// it with Enter when the record can be resumed, and dismiss or destroy it
// from its actions menu.
func (d *DashboardState) stoppedFailureGuidance(sessionID string) string {
	panic("unported: mj-tui/src/dashboard_standby.rs DashboardState::stopped_failure_guidance")
}

// NoticeNamingWorkspace makes a daemon notice about a session in another
// workspace name that workspace first, so a footer that cuts the tail still
// says where to look.
func (d *DashboardState) NoticeNamingWorkspace(sessionID string, text string) string {
	panic("unported: mj-tui/src/dashboard_standby.rs DashboardState::notice_naming_workspace")
}

// TransitionFailureKind is the durable transition that failed before it
// could return to an ordinary state, nil when none.
func (d *DashboardState) TransitionFailureKind(sessionID string) *core.SessionTransitionKind {
	return d.verdict(sessionID).TransitionFailure
}

// DashboardStateTransitionAcceptsPrompts is Rust's
// DashboardState::transition_accepts_prompts: the lifecycle transitions which
// retain an editable prompt.
func DashboardStateTransitionAcceptsPrompts(kind core.SessionTransitionKind) bool {
	return kind == core.SessionTransitionKindStarting ||
		kind == core.SessionTransitionKindResuming ||
		kind == core.SessionTransitionKindMoving
}

// standbyPromptSession is the selected session whose prompt band shows the
// standby composer: a Starting, Resuming, or Moving transition parks the
// conversation behind it, or an attach for the session is in flight.
// Stopping, Destroying, and failed transitions keep the status panel
// instead: there is no conversation to type toward.
func (d *DashboardState) standbyPromptSession() (string, bool) {
	sessionID, ok := d.CurrentSessionID()
	if !ok || d.SessionFailed(sessionID) || d.TransitionFailureKind(sessionID) != nil {
		return "", false
	}
	transition := d.TransitionKind(sessionID)
	parked := (transition != nil && DashboardStateTransitionAcceptsPrompts(*transition)) ||
		(d.openingSession != nil && *d.openingSession == sessionID)
	if !parked {
		return "", false
	}
	return sessionID, true
}

// standbyPromptMut is the standby composer a session's prompt band is
// editing, creating it on first use so every host path (seeding, keys,
// paste, render) shares one instance.
func (d *DashboardState) standbyPromptMut(sessionID string) *chat.ChatState {
	reviewers := d.ReviewerCountsFor(sessionID)
	prompt, ok := d.standbyPrompts[sessionID]
	if !ok {
		prompt = d.buildStandbyPrompt(sessionID)
		d.standbyPrompts[sessionID] = prompt
	}
	prompt.SetReviewerCounts(reviewers)
	return prompt
}

func (d *DashboardState) buildStandbyPrompt(sessionID string) *chat.ChatState {
	panic("unported: mj-tui/src/dashboard_standby.rs DashboardState::build_standby_prompt")
}

// standbyPromptHeader is the session-list columns a standby composer shows
// in its header.
func (d *DashboardState) standbyPromptHeader(sessionID string) chat.SessionHeaderIdentity {
	panic("unported: mj-tui/src/dashboard_standby.rs DashboardState::standby_prompt_header")
}

// SeedStandbyPrompt seeds the standby composer from a warm chat's input, so a
// restart carries the text on screen through the transition instead of
// blanking it.
func (d *DashboardState) SeedStandbyPrompt(sessionID string, text string) {
	if text == "" {
		return
	}
	d.standbyPromptMut(sessionID).SetDraft(text)
}

// StandbyQuestionDrafts drains a session's standby composer and returns its
// question drafts, for the chat open that adopts them.
func (d *DashboardState) StandbyQuestionDrafts(sessionID string) []chat.ChatElicitationDraft {
	composer, ok := d.standbyPrompts[sessionID]
	if !ok {
		return nil
	}
	composer.DrainComposer()
	return composer.ElicitationDrafts()
}

// TakeStandbyPromptDraft removes a session's standby composer and returns its
// draft.
func (d *DashboardState) TakeStandbyPromptDraft(sessionID string) (string, bool) {
	standby, ok := d.standbyPrompts[sessionID]
	if !ok {
		return "", false
	}
	delete(d.standbyPrompts, sessionID)
	return standby.Draft(), true
}

func (d *DashboardState) RetainStartupReceipt(sessionID string, prompt chat.StartupPrompt, inheritedDraft *string) {
	d.standbyPromptMut(sessionID).RetainStartupReceipt(prompt, inheritedDraft)
}

func (d *DashboardState) SettleStartupPreview(sessionID string, commandID string) {
	if standby, ok := d.standbyPrompts[sessionID]; ok {
		standby.SettleStartupPreview(commandID)
	}
}

func (d *DashboardState) RestoreStartupPrompt(sessionID string, prompt chat.StartupPrompt) error {
	return d.standbyPromptMut(sessionID).RestoreStartupPrompt(prompt)
}

// StartupPromptWithdrawn applies the daemon's answer to a withdrawal: Rust's
// Result<bool, String> is (withdrawn, err).
func (d *DashboardState) StartupPromptWithdrawn(sessionID string, prompt chat.StartupPrompt, withdrawn bool, err error) (bool, error) {
	return d.standbyPromptMut(sessionID).StartupPromptWithdrawn(prompt, withdrawn, err)
}

// SetStandbyMoveOrigin is Rust's set_standby_move_origin with its
// Option<(u64, Option<String>)> as a *chat.ComposerMoveOrigin.
func (d *DashboardState) SetStandbyMoveOrigin(sessionID string, origin *chat.ComposerMoveOrigin) {
	d.standbyPromptMut(sessionID).SetComposerMoveOrigin(origin)
}

func (d *DashboardState) SettleStandbyMoveOrigin(sessionID string, generation uint64, id *string, succeeded bool) {
	if standby, ok := d.standbyPrompts[sessionID]; ok {
		standby.SettleDeferredMoveOrigin(generation, id, succeeded)
	}
}

func (d *DashboardState) KeepStandbySubmissionAsDraft(sessionID string) {
	if standby, ok := d.standbyPrompts[sessionID]; ok {
		standby.KeepDeferredSubmissionAsDraft()
	}
}

// StandbySessionIDs lists the sessions with a standby composer, sorted.
func (d *DashboardState) StandbySessionIDs() []string {
	return slices.Sorted(maps.Keys(d.standbyPrompts))
}

func (d *DashboardState) HasStandbyComposer(sessionID string) bool {
	_, ok := d.standbyPrompts[sessionID]
	return ok
}

// StandbyPromptDraft is a snapshot of the standby composer's draft, without
// disturbing pending reads or image processing.
func (d *DashboardState) StandbyPromptDraft(sessionID string) (string, bool) {
	standby, ok := d.standbyPrompts[sessionID]
	if !ok {
		return "", false
	}
	return standby.Draft(), true
}

// TakeStandbyComposer removes a session's standby composer and hands over its
// live composer; nil when there is none.
func (d *DashboardState) TakeStandbyComposer(sessionID string) *chat.LiveComposer {
	standby, ok := d.standbyPrompts[sessionID]
	if !ok {
		return nil
	}
	delete(d.standbyPrompts, sessionID)
	return standby.TakeLiveComposer()
}

func (d *DashboardState) AdoptStandbyComposer(sessionID string, composer *chat.LiveComposer) {
	d.standbyPromptMut(sessionID).AdoptLiveComposer(composer)
}

func (d *DashboardState) StandbyStartupPrompts(sessionID string) []chat.StartupPrompt {
	standby, ok := d.standbyPrompts[sessionID]
	if !ok {
		return nil
	}
	return standby.StartupPrompts()
}

// DrainStandbyComposers drains every composer, including those hidden by
// navigation. A result belongs to its original owner.
func (d *DashboardState) DrainStandbyComposers() (changed bool, actions []DashboardAction) {
	panic("unported: mj-tui/src/dashboard_standby.rs DashboardState::drain_standby_composers")
}

// handleStandbyPromptKey handles keys for the standby composer shown while a
// Starting/Resuming/Moving transition or an in-flight attach owns the
// selected session. It is the real composer, so the whole readline chord set
// edits the draft; only the dashboard's own pane keys are reserved.
// Everything configurable is matched by the prefix router before the key
// reaches here. Enter on a plain prompt returns QueueStartupPrompt so the
// host can have the daemon deliver it when the session is live; a command
// keeps the draft and explains. A non-nil result means the key was consumed,
// including as a no-op.
func (d *DashboardState) handleStandbyPromptKey(key tea.KeyMsg) DashboardAction {
	panic("unported: mj-tui/src/dashboard_standby.rs DashboardState::handle_standby_prompt_key")
}

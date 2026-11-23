// Port of mj-chat/src/chat/status.rs.

package chat

import (
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func (c *ChatState) Phase() core.WorkerPhase {
	return c.phase
}

// AcknowledgeRender records the time-dependent cells represented by the
// frame just drawn. The next clock or animation tick can then request a
// redraw only after its displayed value actually moves.
func (c *ChatState) AcknowledgeRender() {
	panic("unported: mj-chat/src/chat/status.rs AcknowledgeRender")
}

func (c *ChatState) setTranscriptLoading(loading bool) {
	if c.transcriptLoading != loading {
		c.transcriptLoading = loading
	}
}

func (c *ChatState) SetHistoryContext(bundleID string) {
	if c.bundleID == nil || *c.bundleID != bundleID {
		c.bundleID = &bundleID
	}
}

func (c *ChatState) SetSessionModes(modes *schema.SessionModeState) {
	c.acpSurface.SetSessionModes(modes)
	c.rebuildCommandChoices()
}

func (c *ChatState) SetHarnessKind(harnessKind core.HarnessKind) {
	c.acpSurface.SetHarnessKind(harnessKind)
	c.rebuildCommandChoices()
}

func (c *ChatState) supportsPlanMode() bool {
	return c.acpSurface.SupportsPlanMode()
}

func (c *ChatState) supportsFastMode() bool {
	return c.acpSurface.SupportsFastMode()
}

func (c *ChatState) fastModeActive() bool {
	return c.acpSurface.FastModeActive()
}

// planControl is the ACP call that turns plan mode on or off, or the
// user-facing reason it cannot.
func (c *ChatState) planControl(active bool) (core.PlanControl, error) {
	panic("unported: mj-chat/src/chat/status.rs planControl")
}

func (c *ChatState) planModeActive() bool {
	return c.acpSurface.PlanModeActive()
}

func (c *ChatState) beginPlanModeChange(active bool) {
	c.acpSurface.BeginPlanModeChange(active)
}

func (c *ChatState) finishPlanModeChange(active bool) {
	c.acpSurface.FinishPlanModeChange(active)
}

func (c *ChatState) currentModel() (string, bool) {
	return c.acpSurface.CurrentModel()
}

func (c *ChatState) currentEffort() (string, bool) {
	return c.acpSurface.CurrentEffort()
}

func (c *ChatState) planReviewFollowup(request *core.ElicitationRequest, response *core.ElicitationResponse) *PlanReviewFollowup {
	panic("unported: mj-chat/src/chat/status.rs planReviewFollowup")
}

// AdoptSessionID re-points a standby composer at the session that adopted
// it. The launch standby is built before the daemon has registered a
// session, so it starts with a placeholder id that has to be corrected once
// the real id arrives.
func (c *ChatState) AdoptSessionID(sessionID string) {
	if c.sessionID != sessionID {
		c.resetToolDetails()
		c.sessionID = sessionID
	}
	c.BindComposerSession(&sessionID)
}

// SetHeaderSummary installs the stable session-list columns used by the
// conversation title.
func (c *ChatState) SetHeaderSummary(target string, profile string, title string) {
	if c.headerTarget != target || c.headerProfile != profile || c.headerTitle != title {
		c.headerTarget = target
		c.headerProfile = profile
		c.headerTitle = title
	}
}

// HeaderTitle is the session name the conversation's title shows.
func (c *ChatState) HeaderTitle() string {
	return c.headerTitle
}

// setPromptInFlight records whether the session has a prompt of ours in
// flight, which is what the relay accepts a cancellation for.
func (c *ChatState) setPromptInFlight(inFlight bool) {
	if c.promptInFlight != inFlight {
		c.promptInFlight = inFlight
	}
	if c.sessionActivity.PromptInFlight != inFlight {
		c.sessionActivity.PromptInFlight = inFlight
	}
}

// PromptInFlight is Rust's crate-private `prompt_in_flight()`, exported
// because Go cannot give a method the name of the promptInFlight field.
func (c *ChatState) PromptInFlight() bool {
	return c.promptInFlight
}

// harnessTurnStoppable: autonomous harness turns use the same interrupt path
// as prompted turns. Goal controls manage continuation; they do not replace
// turn cancellation.
func (c *ChatState) harnessTurnStoppable() bool {
	return c.sessionActivity.HarnessTurnStartedAtMs != nil || c.goalState.Running()
}

// turnInterruptible reports whether Esc (or the host's Interrupt turn
// command) has a turn to interrupt, including autonomous harness and native
// goal turns.
func (c *ChatState) turnInterruptible() bool {
	panic("unported: mj-chat/src/chat/status.rs turnInterruptible")
}

// turnControlPending reports whether a turn-control request is already on
// its way, when a second interrupt would do nothing.
func (c *ChatState) turnControlPending() bool {
	return c.turnControlSubmitting || c.turnControlAwaitingState != nil || c.cancellingPromptID != nil
}

func (c *ChatState) turnControlIntent() TurnControlIntent {
	panic("unported: mj-chat/src/chat/status.rs turnControlIntent")
}

// setSessionActivity records what the session is doing beyond its phase, so
// the pane title and the composer can name background work.
func (c *ChatState) setSessionActivity(activity client.SessionActivity) {
	panic("unported: mj-chat/src/chat/status.rs setSessionActivity")
}

// SessionActivity is Rust's crate-private `session_activity()`, exported
// because Go cannot give a method the name of the sessionActivity field.
func (c *ChatState) SessionActivity() *client.SessionActivity {
	return &c.sessionActivity
}

func (c *ChatState) backgroundTaskCount() int {
	return len(c.sessionActivity.BackgroundCommands)
}

// SubagentCount is Rust's crate-private `subagent_count()`, exported
// because Go cannot give a method the name of the subagentCount field.
func (c *ChatState) SubagentCount() int {
	return c.subagentCount
}

func (c *ChatState) SetSubagentWorkingCount(count int) {
	c.subagentWorkingCount = count
}

// SubagentWorkingCount is how many sub-agents the composer footer shows as
// working.
func (c *ChatState) SubagentWorkingCount() int {
	return c.subagentWorkingCount
}

func (c *ChatState) SetSubagentCount(count int) {
	if c.subagentCount != count {
		c.subagentCount = count
		if count == 0 && c.subagentControlFocused() {
			c.promptControlFocus = nil
		}
	}
}

func (c *ChatState) subagentControlFocused() bool {
	return c.promptControlFocus != nil && *c.promptControlFocus == PromptControlSubagents
}

func (c *ChatState) focusSubagentControl() {
	if c.subagentCount > 0 && !c.subagentControlFocused() {
		focus := PromptControlSubagents
		c.promptControlFocus = &focus
	}
}

func (c *ChatState) SetReviewerCounts(counts ReviewerCounts) {
	c.reviewers = counts
	if counts.Total == 0 && c.promptControlFocus != nil && *c.promptControlFocus == PromptControlReviewers {
		c.promptControlFocus = nil
	}
}

func (c *ChatState) focusReviewerControl() {
	if c.reviewers.Total > 0 {
		focus := PromptControlReviewers
		c.promptControlFocus = &focus
	}
}

func (c *ChatState) backgroundStopPending(id string) bool {
	panic("unported: mj-chat/src/chat/status.rs backgroundStopPending")
}

// requestBackgroundStop starts a targeted stop request if this task is live,
// stoppable, and not already awaiting a provider response. The caller sends
// the returned action through the supervised remote worker.
func (c *ChatState) requestBackgroundStop(id string) ChatAction {
	panic("unported: mj-chat/src/chat/status.rs requestBackgroundStop")
}

// failBackgroundStop clears a pending stop after the remote provider
// rejected it. A successful acknowledgement intentionally has no
// corresponding call: the next activity snapshot owns removal and keeps the
// row honest.
func (c *ChatState) failBackgroundStop(id string, err string) {
	panic("unported: mj-chat/src/chat/status.rs failBackgroundStop")
}

func (c *ChatState) failAllBackgroundStops() bool {
	panic("unported: mj-chat/src/chat/status.rs failAllBackgroundStops")
}

func (c *ChatState) taskControlFocused() bool {
	return c.promptControlFocus != nil && *c.promptControlFocus == PromptControlTasks
}

// TaskDialogOpen is Rust's crate-private `task_dialog_open()`, exported
// because Go cannot give a method the name of the taskDialogOpen field.
func (c *ChatState) TaskDialogOpen() bool {
	return c.taskDialogOpen
}

func (c *ChatState) closeTaskDialog() {
	panic("unported: mj-chat/src/chat/status.rs closeTaskDialog")
}

func (c *ChatState) focusTaskControl() {
	if c.backgroundTaskCount() > 0 && !c.taskControlFocused() {
		focus := PromptControlTasks
		c.promptControlFocus = &focus
	}
}

func (c *ChatState) openTaskDialog() {
	if c.backgroundTaskCount() > 0 && !c.taskDialogOpen {
		c.taskDialogOpen = true
		c.promptControlFocus = nil
		c.taskDialogScroll = 0
		c.taskDialogMaxScroll = 0
	}
}

func (c *ChatState) setTaskDialogScroll(scroll int) {
	scroll = min(scroll, c.taskDialogMaxScroll)
	if c.taskDialogScroll != scroll {
		c.taskDialogScroll = scroll
	}
}

func (c *ChatState) activateBackgroundTaskControl(control BackgroundTaskControl) ChatAction {
	panic("unported: mj-chat/src/chat/status.rs activateBackgroundTaskControl")
}

func (c *ChatState) cursorIsOnLastPromptLine() bool {
	panic("unported: mj-chat/src/chat/status.rs cursorIsOnLastPromptLine")
}

func (c *ChatState) setCurrentStepStart(timestampMs *int64) {
	panic("unported: mj-chat/src/chat/status.rs setCurrentStepStart")
}

func (c *ChatState) LatestSeq() uint64 {
	return c.latestSeq
}

func (c *ChatState) SetSpinnerStyle(style core.SpinnerStyle) {
	if c.spinnerStyle != style {
		c.spinnerStyle = style
	}
}

func (c *ChatState) SetDetailedActivityClocks(detailed bool) {
	if c.detailedActivityClocks != detailed {
		c.detailedActivityClocks = detailed
	}
}

// SetToolDisplay redraws the transcript when Settings changes how tool calls
// show.
func (c *ChatState) SetToolDisplay(tools ToolDisplay) {
	panic("unported: mj-chat/src/chat/status.rs SetToolDisplay")
}

// NeedsAnimation reports that activity animates only while the session has
// work to report.
func (c *ChatState) NeedsAnimation() bool {
	panic("unported: mj-chat/src/chat/status.rs NeedsAnimation")
}

// ClockChanged reports whether the transcript or task clocks differ from the
// last drawn frame.
func (c *ChatState) ClockChanged() bool {
	return c.lastClockText == nil || *c.lastClockText != c.clockText(core.EpochSeconds())
}

func (c *ChatState) clockText(now uint64) string {
	panic("unported: mj-chat/src/chat/status.rs clockText")
}

// AnimationChanged reports that an animation tick changes the activity
// spinner while work is visible.
func (c *ChatState) AnimationChanged() bool {
	panic("unported: mj-chat/src/chat/status.rs AnimationChanged")
}

func (c *ChatState) ActivitySpinner() Line {
	return ActivityLine(c.spinnerStyle, ElapsedMs(), c.NeedsAnimation())
}

// SetReviewConfig mirrors `[review]` into the view, so `/review status` and
// the composer title report what the daemon is actually armed with.
func (c *ChatState) SetReviewConfig(review core.ReviewConfig) {
	panic("unported: mj-chat/src/chat/status.rs SetReviewConfig")
}

// SetSessionReview mirrors the session's own review choice, which `mj new`
// can set.
func (c *ChatState) SetSessionReview(session *core.SessionRecord) {
	panic("unported: mj-chat/src/chat/status.rs SetSessionReview")
}

// ReviewConfig is `[review]` as it applies to this session: Rust's
// crate-private `review_config()`, exported because Go cannot give a method
// the name of the reviewConfig field.
func (c *ChatState) ReviewConfig() core.ReviewConfig {
	panic("unported: mj-chat/src/chat/status.rs ReviewConfig")
}

func (c *ChatState) markPromptSubmitted(prompt string) {
	c.phase = core.WorkerPhaseRunning
	c.promptInFlight = true
	c.goalPromptActive = promptInvokesCommand(prompt, "goal")
	c.feedback.Clear()
	// Local echo: start the clock now so the header moves with the send.
	// The next materialized update replaces this with the recorded start.
	started := core.EpochSeconds()
	c.turnStartedAtEpochSeconds = &started
}

// startTurnClock starts the header clock for a turn the event log just
// reported. An event with no recorded time falls back to now, because the
// turn is running either way.
func (c *ChatState) startTurnClock(recordedAtMs *int64) {
	panic("unported: mj-chat/src/chat/status.rs startTurnClock")
}

func (c *ChatState) pursuingGoal() bool {
	if c.goalState.Known {
		return c.goalState.Active()
	}
	return c.sessionActivity.PursuingGoal ||
		(c.goalPromptActive && c.acpSurface.AdvertisesCommand("goal"))
}

func (c *ChatState) Entries() []core.ChatEntry {
	return c.entries
}

// MaterializedSession converts a legacy/import transcript projection into
// the controller's canonical logical-session model. Native importers use
// this at their boundary; live relay sessions are projected directly from
// relay events.
func (c *ChatState) MaterializedSession() core.MaterializedSession {
	panic("unported: mj-chat/src/chat/status.rs MaterializedSession")
}

func (c *ChatState) QueuedPromptSnapshot() []core.QueuedPrompt {
	panic("unported: mj-chat/src/chat/status.rs QueuedPromptSnapshot")
}

func (c *ChatState) SetNotice(notice string) {
	c.feedback.Set(notice)
}

func (c *ChatState) clearNotice() {
	panic("unported: mj-chat/src/chat/status.rs clearNotice")
}

// setVoiceAvailable tells the chat whether the host can start voice
// dictation. Availability is separate from voiceActive, because an active
// recording must remain stoppable if the helper later disappears.
func (c *ChatState) setVoiceAvailable(available bool) {
	if c.voiceAvailable != available {
		c.voiceAvailable = available
	}
}

func (c *ChatState) setConnectionNotice(text string) {
	panic("unported: mj-chat/src/chat/status.rs setConnectionNotice")
}

// Notice is feedback local to this conversation, independent of dashboard
// notices.
func (c *ChatState) Notice() (string, bool) {
	panic("unported: mj-chat/src/chat/status.rs Notice")
}

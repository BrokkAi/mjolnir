// Port of mj-chat/src/chat/status.rs.

package chat

import (
	"errors"
	"fmt"
	"slices"
	"strings"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

func (c *ChatState) Phase() core.WorkerPhase {
	return c.phase
}

// AcknowledgeRender records the time-dependent cells represented by the
// frame just drawn. The next clock or animation tick can then request a
// redraw only after its displayed value actually moves.
func (c *ChatState) AcknowledgeRender() {
	clock := c.clockText(core.EpochSeconds())
	c.lastClockText = &clock
	c.lastAnimationFrame = nil
	if c.NeedsAnimation() {
		frame := c.ActivitySpinner()
		c.lastAnimationFrame = &frame
	}
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
	control, failure := c.acpSurface.PlanControl(active)
	if failure != nil {
		return nil, errors.New(planControlErrorMessage(*failure))
	}
	return control, nil
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
	if !core.IsPlanReviewID(request.ID) {
		return nil
	}
	if response.Action != core.ElicitationResponseAccept {
		return &PlanReviewFollowup{desiredActive: true}
	}
	action := "keep_planning"
	if value, ok := response.Content["action"].(core.ElicitationValueString); ok {
		action = string(value)
	}
	var feedback *string
	if value, ok := response.Content["feedback"].(core.ElicitationValueString); ok && strings.TrimSpace(string(value)) != "" {
		text := string(value)
		feedback = &text
	}
	switch action {
	case "implement":
		return &PlanReviewFollowup{desiredActive: false}
	case "exit":
		control, err := c.planControl(false)
		if err != nil {
			control = nil
		}
		return &PlanReviewFollowup{desiredActive: false, control: control}
	case "revise":
		followup := &PlanReviewFollowup{desiredActive: true}
		// Grok carries feedback in its native response. Standard ACP
		// permission responses cannot, so send it as the next planning turn.
		if !core.PlanReviewCarriesNativeFeedback(request.ID) {
			followup.prompt = feedback
		}
		return followup
	}
	return &PlanReviewFollowup{desiredActive: true}
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
	state := c.sessionActivity.State()
	return c.promptInFlight ||
		c.harnessTurnStoppable() ||
		state.LastKnownState().Kind == core.ActivityStateCheckingQuota ||
		(c.sessionActivity.CapacityRetry != nil || c.sessionActivity.QuotaRecovery != nil) ||
		len(c.activeUserShells) > 0
}

// turnControlPending reports whether a turn-control request is already on
// its way, when a second interrupt would do nothing.
func (c *ChatState) turnControlPending() bool {
	return c.turnControlSubmitting || c.turnControlAwaitingState != nil || c.cancellingPromptID != nil
}

func (c *ChatState) turnControlIntent() TurnControlIntent {
	if c.turnControls != nil && c.turnControls.Primary != nil {
		switch c.turnControls.Primary.Kind {
		case core.TurnControlKindSteer:
			return TurnControlIntentSteer
		case core.TurnControlKindApplyQueued:
			return TurnControlIntentApplyQueued
		}
	}
	return TurnControlIntentCancel
}

// setSessionActivity records what the session is doing beyond its phase, so
// the pane title and the composer can name background work.
func (c *ChatState) setSessionActivity(activity client.SessionActivity) {
	// The provider snapshot remains authoritative for task existence. A
	// successful stop acknowledgement does not remove the row itself; only
	// this reconciliation clears its pending label.
	live := make(map[string]struct{}, len(activity.BackgroundCommands))
	for _, command := range activity.BackgroundCommands {
		live[command.ID] = struct{}{}
	}
	for id := range c.pendingBackgroundStops {
		if _, ok := live[id]; !ok {
			delete(c.pendingBackgroundStops, id)
		}
	}
	c.sessionActivity = activity
	if c.backgroundTaskCount() == 0 && c.taskControlFocused() {
		c.promptControlFocus = nil
	}
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
	_, pending := c.pendingBackgroundStops[id]
	return pending
}

// requestBackgroundStop starts a targeted stop request if this task is live,
// stoppable, and not already awaiting a provider response. The caller sends
// the returned action through the supervised remote worker.
func (c *ChatState) requestBackgroundStop(id string) ChatAction {
	stoppable := slices.ContainsFunc(c.sessionActivity.BackgroundCommands, func(command core.BackgroundCommand) bool {
		return command.ID == id && command.CanStop
	})
	if !stoppable || c.backgroundStopPending(id) {
		return ChatActionNone{}
	}
	if c.pendingBackgroundStops == nil {
		c.pendingBackgroundStops = map[string]struct{}{}
	}
	c.pendingBackgroundStops[id] = struct{}{}
	return ChatActionStopBackgroundTask{ID: id}
}

// failBackgroundStop clears a pending stop after the remote provider
// rejected it. A successful acknowledgement intentionally has no
// corresponding call: the next activity snapshot owns removal and keeps the
// row honest.
func (c *ChatState) failBackgroundStop(id string, err string) {
	if c.backgroundStopPending(id) {
		delete(c.pendingBackgroundStops, id)
		c.SetNotice(fmt.Sprintf("Background task could not be stopped: %s", err))
	}
}

func (c *ChatState) failAllBackgroundStops() bool {
	if len(c.pendingBackgroundStops) == 0 {
		return false
	}
	clear(c.pendingBackgroundStops)
	return true
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
	if c.taskDialogOpen || c.taskDialogScroll != 0 || c.taskDialogArea != nil {
		c.taskDialogOpen = false
		c.taskDialogScroll = 0
		c.taskDialogMaxScroll = 0
		c.taskDialogArea = nil
		c.taskDialogControlIDs = nil
		c.taskDialogForm.Clear()
	}
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
	for _, candidate := range c.taskDialogControlIDs {
		if candidate.control == control {
			return c.requestBackgroundStop(candidate.id)
		}
	}
	return ChatActionNone{}
}

func (c *ChatState) cursorIsOnLastPromptLine() bool {
	width := max(c.promptContentWidth, 1)
	_, row := InputCursorVisualPosition(c.input, c.inputCursor, width)
	return row+1 >= InputVisualRows(c.input, width)
}

func (c *ChatState) setCurrentStepStart(timestampMs *int64) {
	var started *uint64
	if timestampMs != nil && *timestampMs >= 0 {
		value := uint64(*timestampMs)
		started = &value
	}
	if !optionalUint64sEqual(c.currentStepStartedAtMs, started) {
		c.currentStepStartedAtMs = started
	}
}

// optionalUint64sEqual compares two Rust `Option<u64>` values.
func optionalUint64sEqual(a, b *uint64) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
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
	if c.toolDisplay != tools {
		c.toolDisplay = tools
		c.invalidateRenderCache()
	}
}

// NeedsAnimation reports that activity animates only while the session has
// work to report.
func (c *ChatState) NeedsAnimation() bool {
	var primaryWorking bool
	switch c.phase {
	case core.WorkerPhaseClosed:
		// A closed worker can leave its last operational snapshot in the chat
		// while that snapshot is being reconciled. Its stale primary activity
		// must not keep the spinner alive after the terminal lifecycle event
		// has settled.
		primaryWorking = false
	case core.WorkerPhaseClosing:
		primaryWorking = true
	default:
		primaryWorking = c.sessionActivity.IsWorking(c.turnStartedAtEpochSeconds, len(c.pendingElicitations) > 0)
	}
	return (c.activityReachable && primaryWorking) || client.AnyWorking(c.reviewViews())
}

// ClockChanged reports whether the transcript or task clocks differ from the
// last drawn frame.
func (c *ChatState) ClockChanged() bool {
	return c.lastClockText == nil || *c.lastClockText != c.clockText(core.EpochSeconds())
}

func (c *ChatState) clockText(now uint64) string {
	// Reuse the title renderer so review/question/connection precedence
	// cannot diverge from the clock that is actually on screen.
	text := transcriptTitle(c, now).String()
	var earliest *int64
	for _, terminal := range c.activeAgentTerminals {
		claimedAt, claimed := c.claimedAgentTerminals[terminal.TerminalID]
		if claimed && claimedAt >= terminal.StartedAtMs {
			continue
		}
		if earliest == nil || terminal.StartedAtMs < *earliest {
			started := terminal.StartedAtMs
			earliest = &started
		}
	}
	if earliest != nil {
		var started *uint64
		if seconds := *earliest / 1_000; seconds >= 0 {
			value := uint64(seconds)
			started = &value
		}
		text += "\x1f" + client.FormatTurnClock(now, started)
	}
	if c.taskDialogOpen {
		for _, command := range c.sessionActivity.BackgroundCommands {
			started := uint64(max(command.StartedAtMs, 0) / 1_000)
			elapsed := uint64(0)
			if now > started {
				elapsed = now - started
			}
			text += "\x1f" + client.FormatClock(elapsed)
		}
	}
	return text
}

// AnimationChanged reports that an animation tick changes the activity
// spinner while work is visible.
func (c *ChatState) AnimationChanged() bool {
	if !c.NeedsAnimation() {
		return c.lastAnimationFrame != nil
	}
	frame := c.ActivitySpinner()
	return c.lastAnimationFrame == nil || !lineEqual(c.lastAnimationFrame, &frame)
}

// lineEqual is Rust's derived PartialEq for Line: spans, style and
// alignment.
func lineEqual(a, b *Line) bool {
	return a.Style == b.Style && a.Alignment == b.Alignment && slices.Equal(a.Spans, b.Spans)
}

func (c *ChatState) ActivitySpinner() Line {
	return ActivityLine(c.spinnerStyle, ElapsedMs(), c.NeedsAnimation())
}

// SetReviewConfig mirrors `[review]` into the view, so `/review status` and
// the composer title report what the daemon is actually armed with.
func (c *ChatState) SetReviewConfig(review core.ReviewConfig) {
	// Rust compares before assigning only to skip an identical write; the
	// assignment has no other effect, so it is unconditional here.
	c.reviewConfig = review
}

// SetSessionReview mirrors the session's own review choice, which `mj new`
// can set.
func (c *ChatState) SetSessionReview(session *core.SessionRecord) {
	c.sessionReview = nil
	if session.Review != nil {
		c.sessionReview = *session.Review
	}
	c.reviewUnavailable = nil
	if reason, unavailable := session.ReviewUnavailable(); unavailable {
		c.reviewUnavailable = &reason
	}
}

// ReviewConfig is `[review]` as it applies to this session: Rust's
// crate-private `review_config()`, exported because Go cannot give a method
// the name of the reviewConfig field.
func (c *ChatState) ReviewConfig() core.ReviewConfig {
	return c.reviewConfig.ForSession(c.sessionReview)
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
	var started uint64
	if recordedAtMs != nil && *recordedAtMs >= 0 {
		started = uint64(*recordedAtMs) / 1_000
	} else {
		started = core.EpochSeconds()
	}
	if c.turnStartedAtEpochSeconds == nil || *c.turnStartedAtEpochSeconds != started {
		c.turnStartedAtEpochSeconds = &started
	}
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
	configuration := map[string]any{}
	if model, ok := c.acpSurface.CurrentModel(); ok {
		configuration["model"] = model
	}
	if effort, ok := c.acpSurface.CurrentEffort(); ok {
		configuration["effort"] = effort
	}
	queued := make([]core.MaterializedQueuedPrompt, 0, len(c.queuedPrompts))
	for _, prompt := range c.queuedPrompts {
		queued = append(queued, core.MaterializedQueuedPrompt{
			AcceptedOrdinal: nil,
			CommandID:       prompt.id,
			Kind:            prompt.kind,
			Content:         promptContentBlocks(prompt.text, prompt.images),
			QueuedAtMs:      0,
		})
	}
	pending := []core.ElicitationRequest{}
	if c.elicitation != nil {
		pending = append(pending, *c.elicitation.request())
	}
	return transcript.MaterializedSessionFromEntries(
		c.sessionID,
		c.entries,
		c.latestSeq,
		c.phase,
		core.SessionConfiguration{Values: configuration},
		queued,
		pending,
	)
}

func (c *ChatState) QueuedPromptSnapshot() []core.QueuedPrompt {
	snapshot := make([]core.QueuedPrompt, 0, len(c.queuedPrompts))
	for _, prompt := range c.queuedPrompts {
		snapshot = append(snapshot, core.QueuedPrompt{
			ID:          prompt.id,
			Kind:        prompt.kind,
			Text:        prompt.text,
			Attachments: []core.Attachment{},
			CreatedAtMs: 0,
		})
	}
	return snapshot
}

func (c *ChatState) SetNotice(notice string) {
	c.feedback.Set(notice)
}

func (c *ChatState) clearNotice() {
	if _, ok := c.feedback.Current(); ok {
		c.feedback.Clear()
	}
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
	sanitized := core.SanitizeTerminalText(text)
	c.connectionFeedback = &sanitized
}

// Notice is feedback local to this conversation, independent of dashboard
// notices.
func (c *ChatState) Notice() (string, bool) {
	if notice, ok := c.feedback.Current(); ok {
		return notice, true
	}
	if c.connectionFeedback != nil {
		return *c.connectionFeedback, true
	}
	return "", false
}

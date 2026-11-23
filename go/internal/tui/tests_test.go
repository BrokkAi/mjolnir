// Port of mj-tui/src/tests.rs.

package tui

import (
	"encoding/json"
	"errors"
	"fmt"
	"reflect"
	"runtime"
	"slices"
	"sort"
	"strings"
	"testing"
	"time"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// reflectSnapshot renders v, following pointers and unexported fields, so a
// test can compare values as Rust's derived PartialEq does, and tell whether
// a value changed in place (Rust compares a clone). Nil and empty slices and
// maps print alike; maps print in key order; functions and channels only by
// whether they are set; a pointer already on the path prints as a cycle.
func reflectSnapshot(v any) string {
	var out strings.Builder
	writeReflectSnapshot(&out, reflect.ValueOf(v), map[uintptr]bool{})
	return out.String()
}

func writeReflectSnapshot(out *strings.Builder, v reflect.Value, path map[uintptr]bool) {
	if !v.IsValid() {
		out.WriteString("<invalid>")
		return
	}
	switch v.Kind() {
	case reflect.Pointer:
		if v.IsNil() {
			out.WriteString("nil")
			return
		}
		address := v.Pointer()
		if path[address] {
			out.WriteString("<cycle>")
			return
		}
		path[address] = true
		out.WriteString("&")
		writeReflectSnapshot(out, v.Elem(), path)
		delete(path, address)
	case reflect.Interface:
		if v.IsNil() {
			out.WriteString("nil")
			return
		}
		fmt.Fprintf(out, "(%s)", v.Elem().Type())
		writeReflectSnapshot(out, v.Elem(), path)
	case reflect.Struct:
		out.WriteString(v.Type().String())
		out.WriteString("{")
		for index := range v.NumField() {
			fmt.Fprintf(out, "%s:", v.Type().Field(index).Name)
			writeReflectSnapshot(out, v.Field(index), path)
			out.WriteString(",")
		}
		out.WriteString("}")
	case reflect.Slice, reflect.Array:
		// A nil slice is an empty Vec, as Rust has no nil one.
		out.WriteString("[")
		for index := range v.Len() {
			writeReflectSnapshot(out, v.Index(index), path)
			out.WriteString(",")
		}
		out.WriteString("]")
	case reflect.Map:
		type entry struct{ key, value string }
		var entries []entry
		for iterator := v.MapRange(); iterator.Next(); {
			var key, value strings.Builder
			writeReflectSnapshot(&key, iterator.Key(), path)
			writeReflectSnapshot(&value, iterator.Value(), path)
			entries = append(entries, entry{key.String(), value.String()})
		}
		sort.Slice(entries, func(a, b int) bool { return entries[a].key < entries[b].key })
		out.WriteString("map[")
		for _, entry := range entries {
			fmt.Fprintf(out, "%s:%s,", entry.key, entry.value)
		}
		out.WriteString("]")
	case reflect.Func, reflect.Chan, reflect.UnsafePointer:
		fmt.Fprintf(out, "%s(set=%v)", v.Kind(), !v.IsNil())
	case reflect.String:
		fmt.Fprintf(out, "%q", v.String())
	case reflect.Bool:
		fmt.Fprintf(out, "%v", v.Bool())
	case reflect.Int, reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64:
		fmt.Fprintf(out, "%d", v.Int())
	case reflect.Uint, reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uintptr:
		fmt.Fprintf(out, "%d", v.Uint())
	case reflect.Float32, reflect.Float64:
		fmt.Fprintf(out, "%v", v.Float())
	case reflect.Complex64, reflect.Complex128:
		fmt.Fprintf(out, "%v", v.Complex())
	default:
		fmt.Fprintf(out, "<%s>", v.Kind())
	}
}

// isDashboardMode is Rust's `matches!(dashboard.mode, Mode::Dashboard)`.
func isDashboardMode(dashboard *DashboardState) bool {
	_, ok := dashboard.mode.(ModeDashboard)
	return ok
}

// stateWith is State::default holding sessions and their sub-agent
// relations.
func stateWith(sessions []core.SessionRecord, subagents []core.SubagentMetadata) core.StateState {
	state := core.NewStateState()
	for _, session := range sessions {
		state.Sessions.Insert(session.ID, session)
	}
	for _, relation := range subagents {
		state.Subagents.Insert(relation.ChildSessionID, relation)
	}
	return state
}

// orderedSessionIDs is the ids of the Sessions list, in order.
func orderedSessionIDs(dashboard *DashboardState) []string {
	var ids []string
	for _, session := range dashboard.orderedSessions() {
		ids = append(ids, session.ID)
	}
	return ids
}

func requireIDs(t *testing.T, got []string, want ...string) {
	t.Helper()
	if !slices.Equal(got, want) {
		t.Fatalf("got %q, want %q", got, want)
	}
}

func selectedSessionIs(dashboard *DashboardState, want string) bool {
	id, ok := dashboard.SelectedSessionID()
	if want == "" {
		return !ok
	}
	return ok && id == want
}

func currentSessionIs(dashboard *DashboardState, want string) bool {
	id, ok := dashboard.CurrentSessionID()
	if want == "" {
		return !ok
	}
	return ok && id == want
}

func typeKeys(dashboard *DashboardState, text string) {
	for _, character := range text {
		dashboard.HandleKey(key(character))
	}
}

func sessionsFilterState(dashboard *DashboardState) *SessionStateFilter {
	filter := *dashboard.sessionsFilter.Get()
	if filter == nil {
		return nil
	}
	return filter.State
}

func requireSessionsFilterState(t *testing.T, dashboard *DashboardState, want SessionStateFilter) {
	t.Helper()
	if state := sessionsFilterState(dashboard); state == nil || *state != want {
		t.Fatalf("sessions filter state %v, want %v", state, want)
	}
}

func noSessionsFilter(dashboard *DashboardState) bool {
	return *dashboard.sessionsFilter.Get() == nil
}

func mutableSession(dashboard *DashboardState, sessionID string) *core.SessionRecord {
	return dashboard.state.Mut().Sessions.GetMut(sessionID)
}

func TestRetainedSubagentsRemainBrowsableAfterProfileMove(t *testing.T) {
	parent := runningSession()
	dashboard := dashboardWithSession(parent)
	var agents []core.NativeAgentView
	for i := range 23 {
		state := core.NativeAgentStateDisconnected
		switch {
		case i < 17:
			state = core.NativeAgentStateCompleted
		case i == 17:
			state = core.NativeAgentStateFailed
		}
		agent := core.NativeAgent{
			OwnerSessionID: parent.ID,
			SessionID:      fmt.Sprintf("child-%d", i),
			Name:           fmt.Sprintf("Child %d", i),
			Task:           "Inspect code",
			State:          state,
			Availability:   core.NativeAgentAvailabilityUnknown,
		}
		agents = append(agents, core.NativeAgentView{
			GenerationOrdinal: 1,
			Projection:        core.MaterializedSessionEmpty(agent.ViewID()),
			Agent:             agent,
		})
	}
	for _, agent := range agents[:17] {
		serveAttention(dashboard, []string{agent.Agent.ViewID()}, core.SessionAttentionIdle)
	}
	serveAttention(dashboard, []string{agents[17].Agent.ViewID()}, core.SessionAttentionFailed)
	for _, agent := range agents[18:] {
		serveAttention(dashboard, []string{agent.Agent.ViewID()}, core.SessionAttentionUnreachable)
	}
	dashboard.SetNativeAgents(agents)
	parent.LastProfile = "destination-profile"
	state := core.NewStateState()
	state.Sessions.Insert(parent.ID, parent)
	dashboard.SetState(state)
	if count := dashboard.SubagentCountFor(parent.ID); count != 23 {
		t.Fatalf("sub-agent count %d", count)
	}
	dashboard.OpenSubagentWorkspace(parent.ID)
	if count := len(dashboard.orderedSessions()); count != 6 {
		t.Fatalf("failed and disconnected native children need attention: %d rows", count)
	}
	dashboard.FocusSessions()
	dashboard.HandleKey(key('a'))
	if count := len(dashboard.orderedSessions()); count != 23 {
		t.Fatalf("All shows every child: %d rows", count)
	}
}

func TestInertPointerMotionIsNotConsumed(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	terminal := newRatatuiTestTerminal(120, 40)
	terminal.draw(func(frame *chat.Frame) { renderDashboardForTest(frame, dashboard) })

	for column := range 120 {
		result := dashboard.HandleEventResult(tea.MouseMotionMsg{X: column, Y: 1})
		if result.Consumed {
			t.Fatalf("pointer motion belongs to the selection (column %d)", column)
		}
	}
}

func TestAClampedSelectionMoveIsStillConsumed(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.SelectActiveSession("session-1")

	result := dashboard.HandleEventResult(key(tea.KeyDown))

	if !result.Consumed {
		t.Fatal("the move is consumed")
	}
	if result.Action != nil {
		t.Fatalf("no action: %#v", result.Action)
	}
}

func TestACursorOnlyTextEditIsConsumedWithoutAnAction(t *testing.T) {
	session := runningSession()
	override := "rename me"
	session.SessionTitleOverride = &override
	dashboard := dashboardWithSession(session)
	dashboard.SelectActiveSession("session-1")
	dashboard.DispatchCommand(CommandIdRenameSession)
	if _, ok := dashboard.mode.(ModeRename); !ok {
		t.Fatalf("mode %#v", dashboard.mode)
	}

	result := dashboard.HandleEventResult(key(tea.KeyLeft))

	if !result.Consumed {
		t.Fatal("the edit is consumed")
	}
	if result.Action != nil {
		t.Fatalf("no action: %#v", result.Action)
	}
}

// openRenameThroughThePalette opens the rename editor the way the surface
// offers it now: the palette chord, type enough of "rename" to pick it out,
// Enter. There is no `e` any more.
func openRenameThroughThePalette(t *testing.T, dashboard *DashboardState) {
	t.Helper()
	dashboard.FocusSessions()
	openPalette(dashboard)
	typeKeys(dashboard, "rename")
	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionNone{})
	if _, ok := dashboard.mode.(ModeRename); !ok {
		t.Fatalf("%#v", dashboard.mode)
	}
}

func TestMinimizedSummaryPanesDoNotOperateOnHiddenRows(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.setDeploymentCapacityTargets([]core.DeploymentCapacityTarget{testCapacityTarget()})

	dashboard.focus = FocusTargets
	dashboard.SetPaneSize(SupportPaneTargets, core.PaneSizeMinimized)
	dashboard.HandleKey(key(tea.KeyDown))
	dashboard.HandleKey(key(tea.KeyEnter))
	if dashboard.capacityIndex != 0 {
		t.Fatalf("capacity index %d", dashboard.capacityIndex)
	}
	if !isDashboardMode(dashboard) {
		t.Fatalf("mode %#v", dashboard.mode)
	}

	dashboard.focus = FocusQuota
	dashboard.SetPaneSize(SupportPaneQuota, core.PaneSizeMinimized)
	dashboard.HandleKey(key(tea.KeyDown))
	dashboard.HandleKey(key('e'))
	if selection := dashboard.selectionFor(FocusQuota); selection != 0 {
		t.Fatalf("quota selection %d", selection)
	}
	if !isDashboardMode(dashboard) {
		t.Fatalf("mode %#v", dashboard.mode)
	}
}

func TestCtrlCIsInertOnAnEmptyDashboardAndEveryPane(t *testing.T) {
	for _, focus := range []Focus{
		FocusSessions,
		FocusPrompt,
		FocusTargets,
		FocusQuota,
		FocusNotifications,
		FocusWorkspaces,
	} {
		dashboard := NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{})
		dashboard.focus = focus

		for range 2 {
			requireDashboardAction(t, dashboard.HandleKey(ctrlKey('c')), DashboardActionNone{}, fmt.Sprintf("Ctrl-C should not quit from %v", focus))
			if !isDashboardMode(dashboard) {
				t.Fatalf("%v: mode %#v", focus, dashboard.mode)
			}
			if dashboard.focus != focus {
				t.Fatalf("%v: focus %v", focus, dashboard.focus)
			}
		}
	}
}

func TestCtrlCCancelsTextModalButIsInertOnNonTextModalControls(t *testing.T) {
	rename := dashboardWithSession(runningSession())
	openRenameThroughThePalette(t, rename)
	requireDashboardAction(t, rename.HandleKey(ctrlKey('c')), DashboardActionNone{})
	if !isDashboardMode(rename) {
		t.Fatalf("mode %#v", rename.mode)
	}
	// A second press remains harmless after the text modal has closed.
	requireDashboardAction(t, rename.HandleKey(ctrlKey('c')), DashboardActionNone{})
	if !isDashboardMode(rename) {
		t.Fatalf("mode %#v", rename.mode)
	}

	newSession := dashboardWithSession(runningSession())
	requireDashboardAction(t, openNewSessionWizard(newSession), DashboardActionNone{})
	if _, ok := newSession.mode.(ModeNew); !ok {
		t.Fatalf("mode %#v", newSession.mode)
	}
	modeBeforeCtrlC := reflectSnapshot(newSession.mode)
	requireDashboardAction(t, newSession.HandleKey(ctrlKey('c')), DashboardActionNone{})
	if reflectSnapshot(newSession.mode) != modeBeforeCtrlC {
		t.Fatal("Ctrl-C changed the wizard")
	}
	// The wizard is still open, and repeated Ctrl-C does not activate a
	// control whose label happens to contain the letter `c`.
	requireDashboardAction(t, newSession.HandleKey(ctrlKey('c')), DashboardActionNone{})
	if _, ok := newSession.mode.(ModeNew); !ok {
		t.Fatalf("mode %#v", newSession.mode)
	}
}

// TestEscapeNeverQuitsTheCombinedSurface: the combined surface is quit with
// the detach chord. A stray Escape must never take the conversation off the
// screen.
func TestEscapeNeverQuitsTheCombinedSurface(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)

	for _, expected := range []Focus{
		FocusSessions,
		FocusPrompt,
		FocusTargets,
		FocusQuota,
		FocusNotifications,
		FocusWorkspaces,
	} {
		if dashboard.focus != expected {
			t.Fatalf("focus %v, want %v", dashboard.focus, expected)
		}
		requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEscape)), DashboardActionNone{}, expected)
		dashboard.HandleKey(key(tea.KeyTab))
	}
}

// dashboardWithLiveSessions builds count live sessions, perProject of them
// in each project, so the compact list's threshold and grouping can be
// exercised.
func dashboardWithLiveSessions(count, perProject int) *DashboardState {
	state := core.NewStateState()
	for index := range count {
		session := stoppedSession()
		session.ID = fmt.Sprintf("session-%d", index)
		session.State = core.SessionStateRunning
		session.CreatedAt = fmt.Sprintf("2026-08-%02dT00:00:00Z", index+1)
		directory := fmt.Sprintf("/projects/p%d", index/max(perProject, 1))
		session.ProjectDirectory = &directory
		state.Sessions.Insert(session.ID, session)
	}
	dashboard := NewDashboardState(config(), state, map[string]client.ProfileQuota{})
	publishFixtureVerdicts(dashboard)
	dashboard.SelectActiveSession("session-0")
	return dashboard
}

// TestTheSelectionSurvivesTheListChangingUnderIt: a session that becomes
// history releases the conversation and selection together; only deliberate
// navigation selects a remaining row.
func TestTheSelectionSurvivesTheListChangingUnderIt(t *testing.T) {
	dashboard := dashboardWithLiveSessions(3, 3)
	dashboard.FocusSessions()
	dashboard.SelectActiveSession("session-1")
	if selected := dashboard.selectedSession(); selected == nil || selected.ID != "session-1" {
		t.Fatalf("selected %#v", selected)
	}

	// Focus moving away and back does not move the selection.
	dashboard.HandleKey(key(tea.KeyTab))
	dashboard.HandleKey(tea.KeyPressMsg{Code: tea.KeyTab, Mod: tea.ModShift})
	if selected := dashboard.selectedSession(); selected == nil || selected.ID != "session-1" {
		t.Fatalf("selected %#v", selected)
	}

	state := *dashboard.state.Get()
	state.Sessions.GetMut("session-1").State = core.SessionStateStopped
	dashboard.SetState(state)
	publishFixtureVerdicts(dashboard)
	if !selectedSessionIs(dashboard, "") {
		t.Fatal("no session is selected")
	}
	if !currentSessionIs(dashboard, "") {
		t.Fatal("no session is current")
	}
	dashboard.HandleKey(key(tea.KeyDown))
	if !selectedSessionIs(dashboard, "session-0") {
		t.Fatal("Down selects session-0")
	}
}

// TestCancelWithNothingInFlightShowsANotice: launch campaign finding A-7: the
// cancel chord with nothing in flight says so instead of doing nothing
// silently.
// Hard-won: 30a93a2: cancel with no pending operation was silent.
func TestCancelWithNothingInFlightShowsANotice(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	requireDashboardAction(t, chord(dashboard, CommandIdCancelOperation), DashboardActionNone{})
	if notice, ok := dashboard.Notice(); !ok || notice != "Nothing to cancel" {
		t.Fatalf("notice %q", notice)
	}
}

// TestAPinnedPaneReleasesASuspendedSessionAndNamesIt: launch campaign
// finding A-10: a pinned pane restored for a suspended session kept trying to
// attach and failed after 15 seconds with a notice that named no session. The
// pane is emptied instead, with a notice that names the session; a failure
// to open names it too.
// Hard-won: 945a2da: suspended pinned panes retried an impossible attach.
func TestAPinnedPaneReleasesASuspendedSessionAndNamesIt(t *testing.T) {
	session := stoppedSession()
	title := "Pinned B"
	session.SessionTitleOverride = &title
	dashboard := dashboardWithSession(session)
	pane := dashboard.BrowsePane()
	if _, ok := dashboard.SplitFocusedPane(chat.DirectionHorizontal, nil); !ok {
		t.Fatal("the split has room")
	}
	sessionID := "session-1"
	dashboard.SetPaneSession(pane, &sessionID)
	if !dashboard.PaneSessionIsSuspended("session-1") {
		t.Fatal("the stopped session is suspended")
	}
	dashboard.ReleaseSuspendedPane(pane, "session-1")
	if _, ok := dashboard.PaneSession(pane); ok {
		t.Fatal("the pane is emptied")
	}
	notice, _ := dashboard.Notice()
	if !strings.Contains(notice, "Session Pinned B is suspended") {
		t.Fatal(notice)
	}

	dashboard.ReportOpenFailure("session-1", "Session opening did not respond within 15 seconds")
	notice, _ = dashboard.Notice()
	if !strings.HasPrefix(notice, "Could not open Session Pinned B: ") {
		t.Fatal(notice)
	}

	// A session being resumed is on its way back and is not released.
	dashboard.BeginSessionOperation("session-1", SessionOperationKindResuming, nil)
	if dashboard.PaneSessionIsSuspended("session-1") {
		t.Fatal("a resuming session is not suspended")
	}
}

// TestAStartingSessionCanBeRenamed: launch campaign finding B-10: the title
// is a record field, not worker state, so a session that is still starting
// can be renamed from the Sessions pane and from its type-ahead composer.
// Hard-won: 81e032e: renaming a Starting session did nothing from the composer.
func TestAStartingSessionCanBeRenamed(t *testing.T) {
	for _, fromPrompt := range []bool{false, true} {
		session := stoppedSession()
		session.State = core.SessionStateProvisioning
		dashboard := dashboardWithSession(session)
		dashboard.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
		if fromPrompt {
			// The pane is assigned immediately, before its chat attaches.
			dashboard.FocusPrompt()
		} else {
			dashboard.FocusSessions()
		}
		chord(dashboard, CommandIdRenameSession)
		editor, ok := dashboard.mode.(ModeRename)
		if !ok {
			notice, _ := dashboard.Notice()
			t.Fatalf("fromPrompt=%v: %#v notice=%q", fromPrompt, dashboard.mode, notice)
		}
		if editor.sessionID != "session-1" {
			t.Fatalf("renaming %q", editor.sessionID)
		}
	}
}

func standbyDraft(dashboard *DashboardState, sessionID string) (string, bool) {
	standby, ok := dashboard.standbyPrompts[sessionID]
	if !ok {
		return "", false
	}
	return standby.Draft(), true
}

func requireStandby(t *testing.T, dashboard *DashboardState, sessionID string) *chat.ChatState {
	t.Helper()
	standby, ok := dashboard.standbyPrompts[sessionID]
	if !ok {
		t.Fatal("standby")
	}
	return standby
}

func requireStartupPrompt[A DashboardActionQueueStartupPrompt | DashboardActionWithdrawStartupPrompt](t *testing.T, action DashboardAction, text string) {
	t.Helper()
	got, ok := action.(A)
	if !ok {
		t.Fatalf("unexpected action: %#v", action)
	}
	var sessionID string
	var prompt chat.StartupPrompt
	switch got := any(got).(type) {
	case DashboardActionQueueStartupPrompt:
		sessionID, prompt = got.SessionID, got.Prompt
	case DashboardActionWithdrawStartupPrompt:
		sessionID, prompt = got.SessionID, got.Prompt
	}
	if sessionID != "session-1" || prompt.Payload.Text != text || prompt.CommandID == "" {
		t.Fatalf("unexpected action: %#v", action)
	}
}

// TestTypingDuringALaunchingTransitionEditsTheStandbyDraft: a launching
// session parks its conversation behind a composer the user can edit; the
// draft, including a readline cursor edit, survives to the chat that opens.
func TestTypingDuringALaunchingTransitionEditsTheStandbyDraft(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)
	dashboard.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
	dashboard.FocusPrompt()

	for _, character := range "hello world" {
		requireDashboardAction(t, dashboard.HandleKey(key(character)), DashboardActionNone{})
	}
	dashboard.HandleKey(altKey('b'))
	dashboard.HandleKey(key('!'))

	if draft, ok := standbyDraft(dashboard, "session-1"); !ok || draft != "hello !world" {
		t.Fatalf("draft %q", draft)
	}
	if draft, ok := dashboard.TakeStandbyPromptDraft("session-1"); !ok || draft != "hello !world" {
		t.Fatalf("taken draft %q", draft)
	}
	if _, ok := dashboard.TakeStandbyPromptDraft("session-1"); ok {
		t.Fatal("the draft was taken")
	}
}

// TestEnterDuringAStartingTransitionQueuesThePromptForDelivery: Enter during
// a starting transition hands the prompt to the host for delivery: the draft
// clears and the text stays visible as a queued preview.
func TestEnterDuringAStartingTransitionQueuesThePromptForDelivery(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)
	dashboard.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
	dashboard.FocusSessions()
	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionNone{})
	if dashboard.Focus() != FocusPrompt {
		t.Fatalf("focus %v", dashboard.Focus())
	}
	if kind, ok := dashboard.SessionOperationKind("session-1"); !ok || kind != SessionOperationKindLaunching {
		t.Fatalf("operation %v", kind)
	}
	dashboard.HandleKey(key('h'))

	requireStartupPrompt[DashboardActionQueueStartupPrompt](t, dashboard.HandleKey(key(tea.KeyEnter)), "h")
	if draft, ok := standbyDraft(dashboard, "session-1"); !ok || draft != "" {
		t.Fatalf("draft %q", draft)
	}
	if queued := requireStandby(t, dashboard, "session-1").QueuedPromptTexts(); !slices.Equal(queued, []string{"h"}) {
		t.Fatalf("queued %q", queued)
	}
}

// TestRecallingAQueuedStartupPromptWithdrawsItAndEnterRequeuesTheEdit:
// recalling a queued prompt with Up takes it back from the daemon: the
// preview goes, the composer holds the text, and Enter queues the edit once.
// Hard-won: 888bd1d: recalling queued text left the daemon step pending.
func TestRecallingAQueuedStartupPromptWithdrawsItAndEnterRequeuesTheEdit(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)
	dashboard.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
	dashboard.FocusPrompt()
	typeKeys(dashboard, "first")
	requireStartupPrompt[DashboardActionQueueStartupPrompt](t, dashboard.HandleKey(key(tea.KeyEnter)), "first")

	requireStartupPrompt[DashboardActionWithdrawStartupPrompt](t, dashboard.HandleKey(key(tea.KeyUp)), "first")
	standby := requireStandby(t, dashboard, "session-1")
	if len(standby.QueuedPromptTexts()) != 0 {
		t.Fatal("the preview is withdrawn")
	}
	if standby.Draft() != "first" {
		t.Fatalf("draft %q", standby.Draft())
	}

	dashboard.HandleKey(key('!'))
	requireStartupPrompt[DashboardActionQueueStartupPrompt](t, dashboard.HandleKey(key(tea.KeyEnter)), "first!")
	standby = requireStandby(t, dashboard, "session-1")
	if queued := standby.QueuedPromptTexts(); !slices.Equal(queued, []string{"first!"}) {
		t.Fatalf("queued %q", queued)
	}
}

// TestARecalledPromptThatWasAlreadySentIsNotKeptAsAnUnchangedDraft: the
// daemon says the prompt had already gone out: the composer drops its
// unchanged copy so Enter cannot send it twice, and keeps an edited one.
func TestARecalledPromptThatWasAlreadySentIsNotKeptAsAnUnchangedDraft(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)
	dashboard.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
	dashboard.FocusPrompt()
	dashboard.HandlePaste("sent")

	prompt := chat.StartupPrompt{
		CommandID: "sent-id",
		Payload:   chat.PromptPayloadText("sent"),
	}
	if changed, err := dashboard.StartupPromptWithdrawn("session-1", prompt, false, nil); err != nil || !changed {
		t.Fatalf("withdrawn %v %v", changed, err)
	}
	if draft := dashboard.standbyPrompts["session-1"].Draft(); draft != "" {
		t.Fatalf("draft %q", draft)
	}

	dashboard.HandlePaste("sent and edited")
	if changed, err := dashboard.StartupPromptWithdrawn("session-1", prompt, false, nil); err != nil || changed {
		t.Fatalf("withdrawn %v %v", changed, err)
	}
	if draft := dashboard.standbyPrompts["session-1"].Draft(); draft != "sent and edited" {
		t.Fatalf("draft %q", draft)
	}
}

// TestAnUnconfirmedWithdrawalShowsThePromptAsQueuedAgain: if the daemon
// cannot be asked, the prompt may still go out, so it shows as queued again
// rather than leaving the person to think the queue is empty.
func TestAnUnconfirmedWithdrawalShowsThePromptAsQueuedAgain(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)
	dashboard.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
	dashboard.FocusPrompt()
	dashboard.HandlePaste("maybe")
	dashboard.HandleKey(key(tea.KeyEnter))
	prompts := dashboard.StandbyStartupPrompts("session-1")
	prompt := prompts[len(prompts)-1]
	dashboard.HandleKey(key(tea.KeyUp))

	if _, err := dashboard.StartupPromptWithdrawn("session-1", prompt, false, errors.New("offline")); err == nil {
		t.Fatal("an unconfirmed withdrawal is an error")
	}

	standby := requireStandby(t, dashboard, "session-1")
	if queued := standby.QueuedPromptTexts(); !slices.Equal(queued, []string{"maybe"}) {
		t.Fatalf("queued %q", queued)
	}
	if standby.Draft() != "" {
		t.Fatalf("draft %q", standby.Draft())
	}
}

// TestARefusedStartupPromptReturnsToTheStandbyDraft: a prompt the daemon
// refused is merged into the newer draft and its preview goes away.
func TestARefusedStartupPromptReturnsToTheStandbyDraft(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)
	dashboard.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
	dashboard.FocusPrompt()
	typeKeys(dashboard, "first")
	dashboard.HandleKey(key(tea.KeyEnter))
	typeKeys(dashboard, "next")

	prompts := dashboard.StandbyStartupPrompts("session-1")
	prompt := prompts[len(prompts)-1]
	if err := dashboard.RestoreStartupPrompt("session-1", prompt); err != nil {
		t.Fatal(err)
	}

	standby := requireStandby(t, dashboard, "session-1")
	if standby.Draft() != "next\n\nfirst" {
		t.Fatalf("draft %q", standby.Draft())
	}
	if len(standby.QueuedPromptTexts()) != 0 {
		t.Fatal("the preview is gone")
	}
}

// TestACommandInTheStandbyComposerKeepsItsDraft: a command cannot be
// answered while the session is offline, so Enter keeps the draft and
// explains instead of queueing anything.
func TestACommandInTheStandbyComposerKeepsItsDraft(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)
	dashboard.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
	dashboard.FocusPrompt()
	typeKeys(dashboard, "/help")

	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionNone{})

	standby := requireStandby(t, dashboard, "session-1")
	if _, ok := standby.Notice(); !ok {
		t.Fatal("the composer explains")
	}
	if _, ok := dashboard.Notice(); ok {
		t.Fatal("the dashboard has no notice")
	}
	if standby.Draft() != "/help" {
		t.Fatalf("draft %q", standby.Draft())
	}
	if len(standby.QueuedPromptTexts()) != 0 {
		t.Fatal("nothing is queued")
	}
}

// TestAStoppingTransitionOffersNoStandbyComposer: retiring transitions have
// no conversation to type toward, so their keys keep falling through to the
// ordinary dashboard handling.
func TestAStoppingTransitionOffersNoStandbyComposer(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)
	dashboard.BeginSessionOperation("session-1", SessionOperationKindSuspending, nil)
	dashboard.FocusPrompt()

	requireDashboardAction(t, dashboard.HandleKey(key('h')), DashboardActionNone{})
	if _, ok := dashboard.standbyPrompts["session-1"]; ok {
		t.Fatal("no standby composer")
	}
}

// TestAMovingTransitionEditsAndRecallsQueuedStandbyPrompts: moving keeps the
// composer available; Enter and recall emit the same queue actions as they
// do during startup.
func TestAMovingTransitionEditsAndRecallsQueuedStandbyPrompts(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)
	dashboard.BeginSessionOperation("session-1", SessionOperationKindMoving, nil)
	dashboard.FocusPrompt()

	dashboard.HandlePaste("first")
	dashboard.HandleKey(key('!'))
	requireStartupPrompt[DashboardActionQueueStartupPrompt](t, dashboard.HandleKey(key(tea.KeyEnter)), "first!")
	requireStartupPrompt[DashboardActionWithdrawStartupPrompt](t, dashboard.HandleKey(key(tea.KeyUp)), "first!")
	dashboard.HandleKey(key('?'))
	requireStartupPrompt[DashboardActionQueueStartupPrompt](t, dashboard.HandleKey(key(tea.KeyEnter)), "first!?")

	standby := requireStandby(t, dashboard, "session-1")
	if standby.Draft() != "" {
		t.Fatalf("draft %q", standby.Draft())
	}
	if queued := standby.QueuedPromptTexts(); !slices.Equal(queued, []string{"first!?"}) {
		t.Fatalf("queued %q", queued)
	}
}

// TestAPasteDuringAStartingTransitionJoinsTheStandbyDraft: a paste into the
// parked composer lands in the draft with terminal line endings normalized,
// the way the chat composer normalizes them.
func TestAPasteDuringAStartingTransitionJoinsTheStandbyDraft(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)
	dashboard.BeginSessionOperation("session-1", SessionOperationKindResuming, nil)
	dashboard.FocusPrompt()

	dashboard.HandlePaste("first\r\nsecond\rthird")

	if draft, ok := standbyDraft(dashboard, "session-1"); !ok || draft != "first\nsecond\nthird" {
		t.Fatalf("draft %q", draft)
	}
}

// Hard-won: c4fbb1a6: a missing session bundle blocked opening the affected session without repair guidance.
func TestOpeningASessionWithMissingConfigurationExplainsRepair(t *testing.T) {
	session := runningSession()
	dashboard := dashboardWithSession(session)
	bundleID := dashboard.selectedSession().BundleID
	bundles := dashboard.config.Mut().Bundles
	bundle, ok := bundles[bundleID]
	if !ok {
		t.Fatal("the fixture has the bundle")
	}
	delete(bundles, bundleID)
	requireDashboardAction(t, dashboard.openSelectedSession(), DashboardActionNone{})
	dialog, ok := dashboard.mode.(ModeConfirm)
	if !ok {
		t.Fatal("repair dialog expected")
	}
	repair, ok := dialog.confirmation.(ConfirmationConfigurationRepair)
	if !ok {
		t.Fatal("repair details expected")
	}
	if !strings.Contains(repair.Error, "missing bundle") || !strings.Contains(repair.Error, "config.toml") {
		t.Fatal(repair.Error)
	}
	dashboard.HandleKey(key(tea.KeyEscape))
	dashboard.config.Mut().Bundles[bundleID] = bundle
	if _, none := dashboard.openSelectedSession().(DashboardActionNone); none {
		t.Fatal("a repaired configuration opens the session")
	}
}

// TestAFreshNoticeSurvivesAKeyPressAndClearsOnceItHasBeenReadable: the
// notice bar is the only report a background failure gets, so a key press
// that happens to arrive while one is fresh must not wipe it.
// Hard-won: 7c56a0f: incidental keys hid a background notice before it rendered.
func TestAFreshNoticeSurvivesAKeyPressAndClearsOnceItHasBeenReadable(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateRunning
	dashboard := dashboardWithSession(session)

	dashboard.SetNotice("Rename failed: relay unreachable")
	shownAt := time.Now()

	requireDashboardAction(t, dashboard.HandleKeyAt(key(tea.KeyDown), shownAt), DashboardActionNone{})
	if notice, _ := dashboard.Notice(); notice != "Rename failed: relay unreachable" {
		t.Fatalf("notice %q", notice)
	}

	requireDashboardAction(t, dashboard.HandleKeyAt(key(tea.KeyDown), shownAt.Add(chat.NoticeMinimumDisplay)), DashboardActionNone{})
	if notice, ok := dashboard.Notice(); ok {
		t.Fatalf("notice %q", notice)
	}
}

func TestTheDetachChordQuitsWithoutMutatingAnyDashboardModal(t *testing.T) {
	newSession := NewDashboardState(config(), core.NewStateState(), map[string]client.ProfileQuota{})
	requireDashboardAction(t, openNewSessionWizard(newSession), DashboardActionNone{})

	resume := dashboardWithSession(stoppedSession())
	requireDashboardAction(t, openResumeWizard(resume), DashboardActionNone{})

	running := stoppedSession()
	running.State = core.SessionStateRunning
	running.Checkpoint = nil
	rename := dashboardWithSession(running)
	openRenameThroughThePalette(t, rename)

	importing := dashboardWithSession(stoppedSession())
	importing.ShowImportProgress("Chosen session")

	confirmImport := dashboardWithSession(stoppedSession())
	confirmImport.ShowImportBundleConfirmation(nil, nil, nil, core.ManagedWorktreeOptions{}, "/work/repo")

	confirm := dashboardWithSession(stoppedSession())
	dialog := newConfirmDialog(ConfirmationForceDestroy{SessionID: "session-1"})
	confirm.mode = ModeConfirm{&dialog}

	resumeDialog := dashboardWithSession(stoppedSession())
	resumeDialog.ShowResumeDialog(1, nil)

	for _, entry := range []struct {
		label     string
		dashboard *DashboardState
	}{
		{"new session", newSession},
		{"resume", resume},
		{"resume dialog", resumeDialog},
		{"rename", rename},
		{"import progress", importing},
		{"import confirmation", confirmImport},
		{"confirmation", confirm},
	} {
		dashboard := entry.dashboard
		if isDashboardMode(dashboard) {
			t.Fatal(entry.label)
		}
		modeBeforeQuit := reflectSnapshot(dashboard.mode)

		// Detach answers from every surface: the event loop routes it before
		// the mode underneath sees the key, so this drives that same path.
		if !dashboard.CommandAllowedNow(CommandIdQuitDetach) {
			t.Fatal(entry.label)
		}
		requireDashboardAction(t, chord(dashboard, CommandIdQuitDetach), DashboardActionQuitDetach{}, entry.label)
		if reflectSnapshot(dashboard.mode) != modeBeforeQuit {
			t.Fatalf("%s: detach changed the mode", entry.label)
		}
	}
}

func TestSessionOrderCacheTracksCreationVisibilityAndWorkspaceChanges(t *testing.T) {
	first := runningSession()
	first.ID = "first"
	first.CreatedAt = "2026-01-01T00:00:00Z"
	second := first
	second.ID = "second"
	second.CreatedAt = "2026-01-02T00:00:00Z"
	dashboard := dashboardWithSession(first)
	dashboard.state.Mut().Sessions.Insert(second.ID, second)
	publishFixtureVerdicts(dashboard)
	requireIDs(t, orderedSessionIDs(dashboard), "first", "second")
	requireIDs(t, orderedSessionIDs(dashboard), "first", "second")
	mutableSession(dashboard, "second").CreatedAt = "2025-01-01T00:00:00Z"
	requireIDs(t, orderedSessionIDs(dashboard), "second", "first")
	mutableSession(dashboard, "second").State = core.SessionStateStopped
	publishFixtureVerdicts(dashboard)
	requireIDs(t, orderedSessionIDs(dashboard), "first")
	mutableSession(dashboard, "second").State = core.SessionStateRunning
	publishFixtureVerdicts(dashboard)
	requireIDs(t, orderedSessionIDs(dashboard), "second", "first")
	mutableSession(dashboard, "second").WorkspaceID = "elsewhere"
	requireIDs(t, orderedSessionIDs(dashboard), "first")
}

// Hard-won: a74134f: workspace moves left a selected session in both tabs.
func TestAFeedRecordWithANewWorkspaceMovesTheRowBetweenTabs(t *testing.T) {
	dashboard := dashboardWithAttentionMix()
	workspace := func(id string) *string { return &id }
	dashboard.SetActiveWorkspace(workspace("default"))
	// The session is open in a pane, so it is the selected one: the list
	// keeps the selected row visible, which must not outlive its workspace.
	dashboard.SelectActiveSession("quiet")
	if !selectedSessionIs(dashboard, "quiet") {
		t.Fatal("quiet is selected")
	}
	if !slices.Contains(orderedSessionIDs(dashboard), "quiet") {
		t.Fatal("quiet is listed")
	}
	next := *dashboard.state.Get()
	next.Sessions.GetMut("quiet").WorkspaceID = "other"
	dashboard.SetState(next)
	if !selectedSessionIs(dashboard, "") {
		t.Fatal("the moved session is no longer selected")
	}
	if slices.Contains(orderedSessionIDs(dashboard), "quiet") {
		t.Fatal("quiet left the tab")
	}
	dashboard.SetActiveWorkspace(workspace("other"))
	if !slices.Contains(orderedSessionIDs(dashboard), "quiet") {
		t.Fatal("quiet is in the other tab")
	}
	dashboard.SetActiveWorkspace(workspace("default"))
	if slices.Contains(orderedSessionIDs(dashboard), "quiet") {
		t.Fatal("quiet is not in the default tab")
	}
	if !selectedSessionIs(dashboard, "") {
		t.Fatal("nothing is selected")
	}
}

func workerRelation(child, parent *core.SessionRecord, taskName, requestKey string, handbackTool bool) core.SubagentMetadata {
	return core.SubagentMetadata{
		Kind:            core.SubagentKindWorker,
		ChildSessionID:  child.ID,
		ParentSessionID: parent.ID,
		TaskName:        taskName,
		ProfileID:       child.LastProfile,
		RequestKey:      requestKey,
		CreatedAt:       child.CreatedAt,
		HandbackTool:    handbackTool,
	}
}

// TestAStoppedSubagentOpensAsItsStoredReadOnlyTranscript: a stopped Mjolnir
// sub-agent has no worker to attach to. Found live: its conversation sat on
// "Bringing your conversation into focus" until the attach timed out, and the
// work it did could not be read. It opens as a read-only view of its stored
// transcript instead.
// Hard-won: bc0cee0: finished child transcripts could not be opened or read.
func TestAStoppedSubagentOpensAsItsStoredReadOnlyTranscript(t *testing.T) {
	parent := stoppedSession()
	parent.ID = "parent-session"
	parent.State = core.SessionStateRunning
	child := stoppedSession()
	child.ID = "child-session"
	title := "Inspect parser"
	child.SessionTitleOverride = &title
	child.State = core.SessionStateStopped
	relation := workerRelation(&child, &parent, "Inspect parser", "request-1", true)
	dashboard := NewDashboardState(
		config(),
		stateWith([]core.SessionRecord{parent, child}, []core.SubagentMetadata{relation}),
		map[string]client.ProfileQuota{},
	)
	// Opening a compact relationship assigns the conversation before row
	// detail arrives. An intermediate refresh must retain that user choice.
	complete := *dashboard.state.Get()
	compact := complete
	compact.Sessions.Remove(child.ID)
	dashboard.SetState(compact)
	dashboard.OpenSubagentWorkspace(parent.ID)
	if !currentSessionIs(dashboard, child.ID) {
		t.Fatal("the child is current")
	}
	dashboard.SetState(compact)
	if !currentSessionIs(dashboard, child.ID) {
		t.Fatal("the refresh keeps the child current")
	}
	parked := complete
	parked.Sessions.GetMut(child.ID).State = core.SessionStateParked
	dashboard.SetState(parked)
	if !dashboard.IsStoppedSubagent(child.ID) {
		t.Fatal("parked children have no worker to attach to")
	}
	dashboard.SetState(complete)
	if !currentSessionIs(dashboard, child.ID) {
		t.Fatal("the child is current")
	}
	if !dashboard.IsStoppedSubagent(child.ID) {
		t.Fatal("the stopped child is read-only")
	}
	if dashboard.IsStoppedSubagent(parent.ID) {
		t.Fatal("the parent is live")
	}
	// The host empties a pane whose session this calls suspended, which
	// would take the read-only view away as soon as Browse showed it.
	if dashboard.PaneSessionIsSuspended(child.ID) {
		t.Fatal("the read-only child is not suspended")
	}

	dashboard.OpenSubagentWorkspace(parent.ID)
	dashboard.FocusSessions()
	dashboard.HandleKey(key('a'))
	dashboard.SelectActiveSession(child.ID)
	requireDashboardAction(t, dashboard.openSelectedSession(), DashboardActionOpen{SessionID: child.ID},
		"Enter reads a stopped child rather than offering to resume it")
	if !dashboard.BeginStoppedSubagent(child.ID) {
		t.Fatal("first open loads it")
	}
	if dashboard.BeginStoppedSubagent(child.ID) {
		t.Fatal("a second open does not load it again")
	}

	draw := func(dashboard *DashboardState) string {
		terminal := newRatatuiTestTerminal(80, 16)
		terminal.draw(func(frame *chat.Frame) {
			area := frame.Area()
			transcript := uv.Rect(0, 0, area.Dx(), 12)
			prompt := uv.Rect(0, 12, area.Dx(), 4)
			pane := dashboard.BrowsePane()
			dashboard.renderStoppedSubagent(frame, pane, "child-session", transcript, prompt)
		})
		return strings.Join(chat.BufferLines(terminal.buffer), "\n")
	}
	if !strings.Contains(draw(dashboard), "Loading this sub-agent") {
		t.Fatal("the read is loading")
	}

	stored := core.MaterializedSessionEmpty(child.ID)
	stored.Transcript = []*core.TranscriptItem{agentMessage(3, "Handed back: the parser drops trailing commas.")}
	dashboard.SetStoppedSubagentTranscript(child.ID, &stored, nil)
	screen := draw(dashboard)
	for _, want := range []string{"Inspect parser · stopped", "parser drops trailing commas", "read-only"} {
		if !strings.Contains(screen, want) {
			t.Fatalf("%q missing from\n%s", want, screen)
		}
	}

	// A failed read says why and loads again on the next open.
	dashboard.SetStoppedSubagentTranscript(child.ID, nil, errors.New("store is busy"))
	if !strings.Contains(draw(dashboard), "store is busy") {
		t.Fatal("the failure is shown")
	}
	if !dashboard.BeginStoppedSubagent(child.ID) {
		t.Fatal("a failed read loads again")
	}
}

// TestASecondSetStateWithANewRelationHidesTheNewChildToo: a daemon-created
// child that arrives through a later runtime snapshot, rather than through a
// full `Controller::load()`, must still hide from the parent's real
// workspace. This is the contract the dashboard's `apply_runtime_records`
// relies on when it assigns `state.subagents` alongside `state.sessions` on
// every `set_state` call.
// Hard-won: 4a99630: runtime snapshots omitted new child relations from workspace filtering.
func TestASecondSetStateWithANewRelationHidesTheNewChildToo(t *testing.T) {
	parent := stoppedSession()
	parent.ID = "parent-session"
	parent.State = core.SessionStateRunning
	firstChild := stoppedSession()
	firstChild.ID = "first-child"
	firstChild.State = core.SessionStateRunning
	firstRelation := workerRelation(&firstChild, &parent, "First task", "request-1", false)
	dashboard := NewDashboardState(
		config(),
		stateWith([]core.SessionRecord{parent, firstChild}, []core.SubagentMetadata{firstRelation}),
		map[string]client.ProfileQuota{},
	)

	publishFixtureVerdicts(dashboard)
	secondChild := stoppedSession()
	secondChild.ID = "second-child"
	secondChild.State = core.SessionStateRunning
	secondRelation := workerRelation(&secondChild, &parent, "Second task", "request-2", false)
	dashboard.setSessionVerdict(secondChild.ID, client.SessionVerdict{LiveDashboard: true})
	dashboard.SetState(stateWith(
		[]core.SessionRecord{parent, firstChild, secondChild},
		[]core.SubagentMetadata{firstRelation, secondRelation},
	))

	if ids := orderedSessionIDs(dashboard); !slices.Equal(ids, []string{parent.ID}) {
		t.Fatalf("a child arriving through set_state alone must still leave the real workspace: %q", ids)
	}

	dashboard.OpenSubagentWorkspace(parent.ID)
	dashboard.FocusSessions()
	dashboard.HandleKey(key('a'))
	virtualWorkspaceIDs := orderedSessionIDs(dashboard)
	slices.Sort(virtualWorkspaceIDs)
	if !slices.Equal(virtualWorkspaceIDs, []string{firstChild.ID, secondChild.ID}) {
		t.Fatalf("both children must appear in the virtual workspace: %q", virtualWorkspaceIDs)
	}
}

// Hard-won: 6eb3146b8: provisioning children were hidden by the default not-idle scope.
func TestSubagentScopeFiltersAttentionWithoutSelectingAnIdleChild(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	template, _ := dashboard.state.Get().Sessions.Get("child-session")
	relation, _ := dashboard.state.Get().Subagents.Get("child-session")
	state := *dashboard.state.Get()
	state.Sessions.Remove("child-session")
	state.Subagents.Remove("child-session")
	for _, child := range []struct {
		id      string
		stopped bool
	}{
		{"a-idle", false},
		{"b-parked", true},
		{"c-working", false},
		{"d-blocked", false},
		{"e-unread", false},
		{"f-provisioning", false},
		{"g-checkpointing", false},
		{"h-closing", false},
		{"i-destroying", false},
	} {
		record := template
		record.ID = child.id
		if child.stopped {
			record.State = core.SessionStateStopped
		}
		childRelation := relation
		childRelation.ChildSessionID = record.ID
		state.Subagents.Insert(record.ID, childRelation)
		state.Sessions.Insert(record.ID, record)
	}
	for _, lifecycle := range []struct {
		id    string
		state core.SessionState
	}{
		{"f-provisioning", core.SessionStateProvisioning},
		{"g-checkpointing", core.SessionStateCheckpointing},
		{"h-closing", core.SessionStateClosing},
		{"i-destroying", core.SessionStateDestroying},
	} {
		state.Sessions.GetMut(lifecycle.id).State = lifecycle.state
	}
	dashboard.SetState(state)
	started := uint64(1)
	mutableSessionDetail(dashboard, "c-working").currentTurnStartedAt = &started
	mutableSessionDetail(dashboard, "d-blocked").pendingElicitations = []core.ElicitationRequest{question("d-blocked")}
	mutableSessionDetail(dashboard, "e-unread").unreadAgentMessages = 1

	dashboard.SetSessionAttentionFor("e-unread", core.SessionAttentionUnread)
	serveAttention(dashboard, []string{"a-idle", "b-parked"}, core.SessionAttentionIdle)
	serveAttention(dashboard, []string{"c-working", "f-provisioning", "g-checkpointing", "h-closing", "i-destroying"}, core.SessionAttentionWorking)
	serveAttention(dashboard, []string{"d-blocked"}, core.SessionAttentionNeedsInput)
	dashboard.FocusSessions()
	dashboard.HandleKey(key('I'))
	if !selectedSessionIs(dashboard, parent) {
		t.Fatal("the parent is selected")
	}
	if count := len(dashboard.orderedSessions()); count != 1 {
		t.Fatalf("idle parent stays on the top-level list: %d rows", count)
	}
	dashboard.OpenSubagentWorkspace(parent)
	requireSessionsFilterState(t, dashboard, SessionStateFilterNotIdle)
	requireIDs(t, orderedSessionIDs(dashboard),
		"c-working", "d-blocked", "e-unread", "f-provisioning", "g-checkpointing", "h-closing", "i-destroying")
	if selectedSessionIs(dashboard, "a-idle") || selectedSessionIs(dashboard, "b-parked") {
		t.Fatal("no hidden child is selected")
	}

	dashboard.HandleKey(key('w'))
	requireIDs(t, orderedSessionIDs(dashboard), "c-working", "f-provisioning", "g-checkpointing", "h-closing", "i-destroying")
	dashboard.HandleKey(key('I'))
	if count := len(orderedSessionIDs(dashboard)); count != 7 {
		t.Fatalf("%d rows", count)
	}

	dashboard.HandleKey(key('a'))
	if count := len(orderedSessionIDs(dashboard)); count != 9 {
		t.Fatalf("All reveals idle and parked children: %d rows", count)
	}
	dashboard.CloseSubagentWorkspace()
	if !selectedSessionIs(dashboard, parent) {
		t.Fatal("the parent is selected")
	}
	requireSessionsFilterState(t, dashboard, SessionStateFilterNotIdle)
	dashboard.OpenSubagentWorkspace(parent)
	if !noSessionsFilter(dashboard) {
		t.Fatal("the child's explicit All survives reopening")
	}
	if count := len(orderedSessionIDs(dashboard)); count != 9 {
		t.Fatalf("%d rows", count)
	}
}

func TestAllIdleSubagentScopeHasNoHiddenChildSelected(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	dashboard.OpenSubagentWorkspace(parent)
	if len(dashboard.orderedSessions()) != 0 {
		t.Fatal("the idle child is hidden")
	}
	if !selectedSessionIs(dashboard, "") {
		t.Fatal("nothing is selected")
	}
	requireSessionsFilterState(t, dashboard, SessionStateFilterNotIdle)
	dashboard.FocusSessions()
	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEscape)), DashboardActionNone{})
	if !noSessionsFilter(dashboard) {
		t.Fatal("Esc clears the filter")
	}
	if len(dashboard.orderedSessions()) != 1 {
		t.Fatal("the child is listed")
	}
	if !selectedSessionIs(dashboard, "") {
		t.Fatal("nothing is selected")
	}

	// An explicit search query in a non-empty child scope still clears on
	// the first Esc, and the following Esc returns to the parent workspace.
	dashboard.HandleKey(key('/'))
	typeKeys(dashboard, "child")
	dashboard.HandleKey(key(tea.KeyEnter))
	if len(dashboard.orderedSessions()) != 1 {
		t.Fatal("the query matches the child")
	}
	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEscape)), DashboardActionNone{})
	if !noSessionsFilter(dashboard) {
		t.Fatal("Esc clears the query")
	}
	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEscape)), DashboardActionExitSubagentWorkspace{})
}

func TestNestedSubagentScopesRestoreEachParentFilterAndText(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	grandchild, _ := dashboard.state.Get().Sessions.Get("child-session")
	grandchild.ID = "grandchild"
	relation, _ := dashboard.state.Get().Subagents.Get("child-session")
	relation.ChildSessionID = grandchild.ID
	relation.ParentSessionID = "child-session"
	state := *dashboard.state.Get()
	state.Subagents.Insert(grandchild.ID, relation)
	state.Sessions.Insert(grandchild.ID, grandchild)
	dashboard.SetState(state)

	dashboard.FocusSessions()
	dashboard.HandleKey(key('I'))
	dashboard.HandleKey(key('/'))
	typeKeys(dashboard, "session")
	dashboard.HandleKey(key(tea.KeyEnter))
	topFilter := dashboard.sessionsFilterLabel()
	if !strings.Contains(topFilter, "/session") || !strings.Contains(topFilter, "not idle") {
		t.Fatal(topFilter)
	}

	dashboard.OpenSubagentWorkspace(parent)
	dashboard.HandleKey(key('a'))
	if !noSessionsFilter(dashboard) {
		t.Fatal("All clears the child filter")
	}
	dashboard.OpenSubagentWorkspace("child-session")
	requireSessionsFilterState(t, dashboard, SessionStateFilterNotIdle)
	dashboard.HandleKey(key('b'))
	dashboard.CloseSubagentWorkspace()
	if id, ok := dashboard.SubagentParentID(); !ok || id != parent {
		t.Fatalf("parent %q", id)
	}
	if !noSessionsFilter(dashboard) {
		t.Fatal("the parent scope keeps All")
	}
	dashboard.OpenSubagentWorkspace("child-session")
	requireSessionsFilterState(t, dashboard, SessionStateFilterBlocked)
	dashboard.CloseSubagentWorkspace()
	dashboard.CloseSubagentWorkspace()
	if label := dashboard.sessionsFilterLabel(); label != topFilter {
		t.Fatalf("label %q", label)
	}
	if !selectedSessionIs(dashboard, parent) {
		t.Fatal("the parent is selected")
	}
}

func TestWorkerAndReviewerScopesForSameParentKeepIndependentFilters(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	reviewer, _ := dashboard.state.Get().Sessions.Get("child-session")
	reviewer.ID = "reviewer-session"
	relation, _ := dashboard.state.Get().Subagents.Get("child-session")
	relation.ChildSessionID = reviewer.ID
	relation.Kind = core.SubagentKindReviewer
	state := *dashboard.state.Get()
	state.Subagents.Insert(reviewer.ID, relation)
	state.Sessions.Insert(reviewer.ID, reviewer)
	dashboard.SetState(state)

	dashboard.OpenSubagentWorkspace(parent)
	requireSessionsFilterState(t, dashboard, SessionStateFilterNotIdle)
	dashboard.HandleKey(key('a'))
	if !noSessionsFilter(dashboard) {
		t.Fatal("All clears the filter")
	}

	dashboard.OpenReviewerWorkspace(parent)
	if !dashboard.ReviewersWorkspaceOpen() {
		t.Fatal("the reviewer workspace is open")
	}
	if !noSessionsFilter(dashboard) {
		t.Fatal("reviewer history defaults to All")
	}
	dashboard.HandleKey(key('b'))
	requireSessionsFilterState(t, dashboard, SessionStateFilterBlocked)

	dashboard.OpenSubagentWorkspace(parent)
	if !noSessionsFilter(dashboard) {
		t.Fatal("worker All survives reviewer entry")
	}
	dashboard.OpenReviewerWorkspace(parent)
	requireSessionsFilterState(t, dashboard, SessionStateFilterBlocked)
}

func workspaceID(id string) *string { return &id }

func requirePaneSize(t *testing.T, dashboard *DashboardState, pane SupportPane, want core.PaneSize) {
	t.Helper()
	if size := dashboard.PaneSize(pane); size != want {
		t.Fatalf("%v pane size %v, want %v", pane, size, want)
	}
}

func TestWorkspaceSwitchRestoresSelectionAndPaneLayoutWithoutLosingLocalEdits(t *testing.T) {
	local := runningSession()
	local.ID = "local"
	remote := runningSession()
	remote.ID = "remote"
	remote.WorkspaceID = "remote-workspace"
	dashboard := dashboardWithSession(local)
	dashboard.state.Mut().Sessions.Insert(remote.ID, remote)
	dashboard.SelectActiveSession("local")
	dashboard.SetPaneSize(SupportPaneSessions, core.PaneSizeMinimized)

	dashboard.CacheWorkspacePaneSizes("remote-workspace", core.PaneSizes{
		Sessions: core.PaneSizeStandard,
		Targets:  core.PaneSizeMaximized,
		Quota:    core.PaneSizeStandard,
	})
	dashboard.SetActiveWorkspace(workspaceID("remote-workspace"))
	dashboard.SelectActiveSession("remote")
	requirePaneSize(t, dashboard, SupportPaneTargets, core.PaneSizeMaximized)
	requirePaneSize(t, dashboard, SupportPaneSessions, core.PaneSizeMinimized)
	dashboard.SetPaneSize(SupportPaneSessions, core.PaneSizeStandard)
	dashboard.CacheWorkspacePaneSizes("remote-workspace", core.PaneSizes{
		Sessions: core.PaneSizeMinimized,
		Targets:  core.PaneSizeStandard,
		Quota:    core.PaneSizeStandard,
	})
	requirePaneSize(t, dashboard, SupportPaneSessions, core.PaneSizeStandard)

	dashboard.SetActiveWorkspace(workspaceID(core.DefaultWorkspaceID))
	if !selectedSessionIs(dashboard, "local") {
		t.Fatal("the default workspace restores its selection")
	}
	requirePaneSize(t, dashboard, SupportPaneSessions, core.PaneSizeStandard)
	dashboard.SetPaneSize(SupportPaneSessions, core.PaneSizeMaximized)
	dashboard.SetActiveWorkspace(workspaceID("remote-workspace"))
	requirePaneSize(t, dashboard, SupportPaneSessions, core.PaneSizeMaximized)
	requirePaneSize(t, dashboard, SupportPaneTargets, core.PaneSizeStandard)
	if err := dashboard.PaneSizes().Validate(); err != nil {
		t.Fatal(err)
	}
	dashboard.SetActiveWorkspace(workspaceID("new-workspace"))
	requirePaneSize(t, dashboard, SupportPaneSessions, core.PaneSizeMaximized)
	dashboard.SetActiveWorkspace(nil)
	requirePaneSize(t, dashboard, SupportPaneSessions, core.PaneSizeMaximized)
}

func frontiers(pairs ...client.DaemonPair[string, uint64]) core.SnapshotMap[string, uint64] {
	frontiers := core.NewSnapshotMap[string, uint64]()
	for _, pair := range pairs {
		frontiers.Insert(pair.First, pair.Second)
	}
	return frontiers
}

func receipt(sessionID string, ordinal uint64) client.DaemonPair[string, uint64] {
	return client.DaemonPair[string, uint64]{First: sessionID, Second: ordinal}
}

func dismissals(sessionID string, target core.NotificationDismissal) core.SnapshotMap[string, core.NotificationDismissal] {
	targets := core.NewSnapshotMap[string, core.NotificationDismissal]()
	if sessionID != "" {
		targets.Insert(sessionID, target)
	}
	return targets
}

func requireAttention(t *testing.T, dashboard *DashboardState, sessionID string, want AttentionLevel) {
	t.Helper()
	if level := dashboard.AttentionLevel(sessionID); level != want {
		t.Fatalf("%s attention %s, want %s", sessionID, attentionLevelName(level), attentionLevelName(want))
	}
}

func viewedThrough(dashboard *DashboardState, sessionID string) uint64 {
	return dashboard.state.Get().Sessions.GetShared(sessionID).ViewedThroughEventOrdinal
}

// Hard-won: 19a4148: mark-all-read cleared unread state in other workspaces.
// Hard-won: 92684ad3b: mark-read skipped output hidden behind working attention.
func TestMarkAllReadLeavesAnotherWorkspacesSessionUnread(t *testing.T) {
	dashboard := dashboardWithAttentionMix()
	dashboard.SetActiveWorkspace(workspaceID("default"))
	for _, id := range []string{"done", "remote"} {
		applyMaterializedTranscriptFor(dashboard, id, []*core.TranscriptItem{agentMessage(4, "unread response")})
		served, want := core.SessionAttentionUnread, AttentionLevelUnread
		if id == "done" {
			served, want = core.SessionAttentionWorking, AttentionLevelWorking
		}
		dashboard.SetSessionAttentionFor(id, served)
		requireAttention(t, dashboard, id, want)
	}

	dashboard.SetUnreadFrontiers(frontiers(receipt("done", 4), receipt("remote", 4)))

	requireDashboardAction(t, chord(dashboard, CommandIdMarkAllRead), DashboardActionMarkAllRead{
		Receipts: []client.DaemonPair[string, uint64]{receipt("done", 4)},
	})
	if dashboard.AttentionLevel("remote") != AttentionLevelUnread {
		t.Fatal("a session in another workspace keeps its unread marker")
	}
	if viewed := viewedThrough(dashboard, "remote"); viewed != 0 {
		t.Fatalf("remote viewed through %d", viewed)
	}
	if viewed := viewedThrough(dashboard, "done"); viewed != 4 {
		t.Fatalf("done viewed through %d", viewed)
	}
	dashboard.SetSessionAttentionFor("done", core.SessionAttentionIdle)
	dashboard.SetUnreadFrontiers(frontiers(receipt("remote", 4)))
	requireAttention(t, dashboard, "done", AttentionLevelIdle)
	dashboard.SetSessionAttentionFor("done", core.SessionAttentionImportant)
	dashboard.SetNotificationDismissals(dismissals("done", core.NotificationDismissalReadThrough{Value: 5}))
	requireDashboardAction(t, chord(dashboard, CommandIdMarkAllRead), DashboardActionMarkAllRead{
		Receipts: []client.DaemonPair[string, uint64]{receipt("done", 5)},
	})
}

func liveSessionsState(count int) core.StateState {
	state := core.NewStateState()
	for index := range count {
		session := stoppedSession()
		session.ID = fmt.Sprintf("session-%d", index)
		session.State = core.SessionStateRunning
		state.Sessions.Insert(session.ID, session)
	}
	return state
}

func selectedVisibleIndexOr(dashboard *DashboardState, fallback int) int {
	if index, ok := dashboard.selectedVisibleIndex(); ok {
		return index
	}
	return fallback
}

var (
	wheelDown  = tea.MouseWheelMsg{Button: tea.MouseWheelDown}
	wheelUp    = tea.MouseWheelMsg{Button: tea.MouseWheelUp}
	rightClick = tea.MouseClickMsg{Button: tea.MouseRight}
)

// Hard-won: 233796e: wheel input scrolled the focused pane instead of the hovered pane.
func TestMouseWheelScrollsTheHoveredPaneWithoutChangingFocus(t *testing.T) {
	dashboard := NewDashboardState(config(), liveSessionsState(5), map[string]client.ProfileQuota{})
	dashboard.SelectActiveSession("session-0")
	terminal := newRatatuiTestTerminal(120, 40)
	terminal.draw(func(frame *chat.Frame) { renderDashboardForTest(frame, dashboard) })
	if dashboard.paneAreas == nil {
		t.Fatal("dashboard pane hitboxes")
	}
	paneAreas := *dashboard.paneAreas

	// The sessions pane scrolls its list under the wheel and keeps the
	// selection.
	dashboard.HandleMouse(mouseIn(wheelDown, paneAreas[0]))
	if index := selectedVisibleIndexOr(dashboard, 0); index != 0 {
		t.Fatalf("selected %d", index)
	}
	if dashboard.sessionsScroll != 1 {
		t.Fatalf("sessions scroll %d", dashboard.sessionsScroll)
	}
	dashboard.HandleMouse(mouseIn(wheelUp, paneAreas[0]))
	if index := selectedVisibleIndexOr(dashboard, 0); index != 0 {
		t.Fatalf("selected %d", index)
	}
	if dashboard.sessionsScroll != 0 {
		t.Fatalf("sessions scroll %d", dashboard.sessionsScroll)
	}

	// The quota wheel scrolls lines without moving either selection.
	if dashboard.focus != FocusSessions {
		t.Fatalf("focus %v", dashboard.focus)
	}
	dashboard.HandleMouse(mouseIn(wheelDown, paneAreas[2]))
	if selection := dashboard.selectionFor(FocusQuota); selection != 0 {
		t.Fatalf("quota selection %d", selection)
	}
	if dashboard.quotaScroll != 1 {
		t.Fatalf("quota scroll %d", dashboard.quotaScroll)
	}
	if index := selectedVisibleIndexOr(dashboard, 0); index != 0 {
		t.Fatalf("selected %d", index)
	}
	if dashboard.focus != FocusSessions {
		t.Fatalf("focus %v", dashboard.focus)
	}
}

func sessionRowArea(t *testing.T, dashboard *DashboardState, index int) uv.Rectangle {
	t.Helper()
	for _, row := range dashboard.sessionRowAreas {
		if row.First == index {
			return row.Second
		}
	}
	t.Fatalf("row %d has a recorded hitbox", index)
	return uv.Rectangle{}
}

func TestClicksOnDifferentRowsDoNotCountAsADoubleClick(t *testing.T) {
	dashboard := dashboardWithConversations(3)
	terminal := newRatatuiTestTerminal(120, 40)
	terminal.draw(func(frame *chat.Frame) { renderDashboardForTest(frame, dashboard) })

	firstRow := sessionRowArea(t, dashboard, 0)
	secondRow := sessionRowArea(t, dashboard, 1)

	requireDashboardAction(t, dashboard.HandleMouse(mouseAtRow(leftButtonDown, firstRow, 0)), DashboardActionNone{})

	// A click on a different row is a fresh first click, not the second half
	// of a double click on row 0.
	requireDashboardAction(t, dashboard.HandleMouse(mouseAtRow(leftButtonDown, secondRow, 0)), DashboardActionNone{})
	if index := selectedVisibleIndexOr(dashboard, 0); index != 1 {
		t.Fatalf("the second click's row is selected: %d", index)
	}
}

// TestRightClickOnASessionRowOpensItsContextMenu: right-clicking a session
// row opens a small menu for that row's session, anchored at the click,
// without moving the selection. The full session menu stays behind the row's
// menu button.
func TestRightClickOnASessionRowOpensItsContextMenu(t *testing.T) {
	dashboard := dashboardWithConversations(3)
	terminal := newRatatuiTestTerminal(120, 40)
	terminal.draw(func(frame *chat.Frame) { renderDashboardForTest(frame, dashboard) })
	if len(dashboard.sessionRowAreas) < 2 {
		t.Fatal("second row has a hitbox")
	}
	secondRow := dashboard.sessionRowAreas[1].Second
	selected, hadSelection := dashboard.selectedVisibleIndex()
	requireDashboardAction(t, dashboard.HandleMouse(mouseAtRow(rightClick, secondRow, 0)), DashboardActionNone{})
	if !isDashboardMode(dashboard) {
		t.Fatal("the full session menu stays closed")
	}
	if dashboard.paneMenu == nil {
		t.Fatal("the row's context menu opened")
	}
	lines := drawn(dashboard, 120, 40)
	var rows []int
	for _, label := range []string{"Dismiss notification", "Copy session ID", "New session with these settings"} {
		rows = append(rows, point(lines, label).Y)
	}
	if !slices.IsSorted(rows) || rows[0] == rows[1] || rows[1] == rows[2] {
		t.Fatalf("%v", rows)
	}
	if index, ok := dashboard.selectedVisibleIndex(); ok != hadSelection || index != selected {
		t.Fatalf("selection moved to %d", index)
	}
}

func holders(pairs ...[2]string) core.SnapshotMap[string, string] {
	holders := core.NewSnapshotMap[string, string]()
	for _, pair := range pairs {
		holders.Insert(pair[0], pair[1])
	}
	return holders
}

// Hard-won: 91244ea: an unopened child held the approval behind a disabled Dismiss row.
func TestRowMenuOpensServedHolderWithoutChildDetailsAndRechecksOnActivation(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	delete(*dashboard.sessionDetails.Mut(), "child-session")
	child := "child-session"
	dashboard.SetCurrentSession(&child)
	serveAttention(dashboard, []string{parent, "child-session"}, core.SessionAttentionNeedsInput)
	dashboard.SetCurrentSession(nil)
	dashboard.SetFormHolders(holders([2]string{parent, "child-session"}, [2]string{"child-session", "child-session"}))
	if waiting, ok := dashboard.waitingChildOf(parent); !ok || waiting != "child-session" {
		t.Fatalf("waiting child %q", waiting)
	}
	dashboard.SetCurrentSession(&child)
	dashboard.beginSessionRowMenu(parent, uv.Pos(1, 1))
	lines := drawn(dashboard, 120, 40)
	if !anyLineContains(lines, "Open sub-agent question") {
		t.Fatalf("%#v", lines)
	}
	if anyLineContains(lines, "Dismiss notification") {
		t.Fatalf("%#v", lines)
	}
	dashboard.SetFormHolders(core.NewSnapshotMap[string, string]())
	if _, ok := dashboard.waitingChildOf(parent); ok {
		t.Fatal("no child is waiting")
	}
	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionNone{})
	if dashboard.paneMenu == nil {
		t.Fatal("stale menu entry is disabled")
	}
	dashboard.SetFormHolders(holders([2]string{parent, "child-session"}, [2]string{"child-session", "child-session"}))
	dashboard.HandleKey(key(tea.KeyEnter))
	if dashboard.paneMenu != nil {
		t.Fatal("the menu closed")
	}
	if !selectedSessionIs(dashboard, "child-session") {
		t.Fatal("the child is selected")
	}
	dashboard.SetFormHolders(holders([2]string{parent, parent}))
	if _, ok := dashboard.waitingChildOf(parent); ok {
		t.Fatal("no child is waiting")
	}
	dashboard.beginSessionRowMenu(parent, uv.Pos(1, 1))
	lines = drawn(dashboard, 120, 40)
	if !anyLineContains(lines, "Dismiss notification") {
		t.Fatalf("%#v", lines)
	}
}

// dashboardWithConversations is a dashboard with count running sessions,
// each carrying a numbered conversation long enough to scroll.
func dashboardWithConversations(count int) *DashboardState {
	dashboard := NewDashboardState(config(), liveSessionsState(count), map[string]client.ProfileQuota{})
	publishFixtureVerdicts(dashboard)
	transcript := numberedConversation(14)
	for index := range count {
		applyMaterializedTranscriptFor(dashboard, fmt.Sprintf("session-%d", index), slices.Clone(transcript))
	}
	return dashboard
}

func TestNewlyReadySessionCanBeSelectedAfterStateRefresh(t *testing.T) {
	newSession := stoppedSession()
	newSession.ID = "new-session"
	newSession.State = core.SessionStateRunning
	other := stoppedSession()
	other.ID = "other"
	other.State = core.SessionStateRunning
	dashboard := NewDashboardState(config(), stateWith([]core.SessionRecord{other}, nil), map[string]client.ProfileQuota{})
	dashboard.focus = FocusQuota

	refreshed := *dashboard.state.Get()
	refreshed.Sessions.Insert(newSession.ID, newSession)
	dashboard.SetState(refreshed)
	dashboard.SelectActiveSession("new-session")

	// Selecting a session no longer steals the keyboard: the caller decides
	// where focus belongs, so a background arrival cannot pull it out of the
	// composer.
	if dashboard.focus != FocusQuota {
		t.Fatalf("focus %v", dashboard.focus)
	}
	if selected := dashboard.selectedSession(); selected == nil || selected.ID != "new-session" {
		t.Fatalf("selected %#v", selected)
	}
}

func requireRecoverFailed(t *testing.T, dashboard *DashboardState, want ConfirmationRecoverFailed) {
	t.Helper()
	dialog, ok := dashboard.mode.(ModeConfirm)
	if !ok {
		t.Fatalf("expected the failed-session prompt, got %#v", dashboard.mode)
	}
	if reflectSnapshot(dialog.confirmation) != reflectSnapshot(Confirmation(want)) {
		t.Fatalf("confirmation %#v, want %#v", dialog.confirmation, want)
	}
}

func optionalText(text string) *string { return &text }

// millis is an optional millisecond timestamp.
func millis(value uint64) *uint64 { return &value }

// TestEnterOnAFailedSessionOffersRecoveryAndTheTranscript: a failed session
// offers both inspection and recovery.
func TestEnterOnAFailedSessionOffersRecoveryAndTheTranscript(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateError
	session.LastError = optionalText("worker bootstrap failed: upload failed")
	dashboard := dashboardWithSession(session)
	// The failure can arrive before an outstanding attach finishes.
	dashboard.SetOpeningSession(optionalText("session-1"))

	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionNone{})
	requireRecoverFailed(t, dashboard, ConfirmationRecoverFailed{
		SessionID:   "session-1",
		Error:       optionalText("worker bootstrap failed: upload failed"),
		Recoverable: true,
		RetryLaunch: false,
	})

	// The prompt draws, names what failed, and offers both answers.
	terminal := newRatatuiTestTerminal(120, 30)
	terminal.draw(func(frame *chat.Frame) { renderDashboardForTest(frame, dashboard) })
	rendered := strings.Join(bufferLines(terminal.buffer), "\n")
	for _, want := range []string{"Session failed", "worker bootstrap failed", "Open transcript", "Recover"} {
		if !strings.Contains(rendered, want) {
			t.Fatalf("%q missing from\n%s", want, rendered)
		}
	}

	// Reading what it did changes nothing about the session.
	dashboard.HandleKey(key(tea.KeyLeft))
	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionOpen{SessionID: "session-1"})
	if dashboard.focus != FocusPrompt {
		t.Fatalf("focus %v", dashboard.focus)
	}
}

// TestAFailedSessionWithoutARecoveryCopyIsNotOfferedRecovery: an absent
// environment with no archive cannot be recovered.
func TestAFailedSessionWithoutARecoveryCopyIsNotOfferedRecovery(t *testing.T) {
	session := stoppedSession()
	session.Target = nil
	session.State = core.SessionStateError
	session.Checkpoint = nil
	session.LastError = optionalText("worker bootstrap failed")
	dashboard := dashboardWithSession(session)

	dashboard.HandleKey(key(tea.KeyEnter))
	requireRecoverFailed(t, dashboard, ConfirmationRecoverFailed{
		SessionID:   "session-1",
		Error:       optionalText("worker bootstrap failed"),
		Recoverable: false,
		RetryLaunch: false,
	})
}

func TestRetryLaunchFromTheFailedSessionDialogUsesTheSavedDestination(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateError
	session.Checkpoint = nil
	session.Target = nil
	expected := DashboardActionResumeSession{
		SessionID:          session.ID,
		WorkspaceID:        session.WorkspaceID,
		ProfileID:          session.LastProfile,
		TargetTemplateID:   session.TargetTemplateID,
		AdditionalMounts:   slices.Clone(session.AdditionalMounts),
		ResourceAllocation: session.ResourceAllocation,
		DiscardQueue:       false,
	}
	id := session.ID
	dashboard := dashboardWithSession(session)
	verdict := dashboard.verdict(id)
	verdict.Resume = client.SessionOfferReady
	dashboard.setSessionVerdict(id, verdict)
	dashboard.SetFailedLaunchRetries(map[string]struct{}{id: {}})

	dashboard.HandleKey(key(tea.KeyEnter))
	terminal := newRatatuiTestTerminal(120, 30)
	terminal.draw(func(frame *chat.Frame) { renderDashboardForTest(frame, dashboard) })
	rendered := strings.Join(bufferLines(terminal.buffer), "\n")
	if !strings.Contains(rendered, "Retry launch") {
		t.Fatal(rendered)
	}

	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEnter)), expected)
	if _, ok := dashboard.mode.(ModeResume); ok {
		t.Fatal("retry launch does not open the resume wizard")
	}
}

// Hard-won: 94d7 resume: failed sessions could retain current work without an archive.
func TestAFailedSessionWithARetainedTargetOffersResumeWithoutAnArchive(t *testing.T) {
	session := stoppedSession()
	session.State = core.SessionStateError
	session.Checkpoint = nil
	session.Target = core.TargetLocatorLocalBare{WorkerRoot: "/retained/session-1"}
	dashboard := dashboardWithSession(session)
	verdict := dashboard.verdict("session-1")
	verdict.Resume = client.SessionOfferReady
	dashboard.setSessionVerdict("session-1", verdict)
	dashboard.HandleKey(key(tea.KeyEnter))
	dialog, ok := dashboard.mode.(ModeConfirm)
	if !ok {
		t.Fatalf("mode %#v", dashboard.mode)
	}
	if recover, ok := dialog.confirmation.(ConfirmationRecoverFailed); !ok || !recover.Recoverable {
		t.Fatalf("confirmation %#v", dialog.confirmation)
	}
	dashboard.BeginResumeFor("session-1")
	if _, ok := dashboard.mode.(ModeResume); !ok {
		t.Fatalf("mode %#v", dashboard.mode)
	}
}

// dashboardWithTwoSessions is two running sessions in the active workspace,
// so a pane can be given one of them and a split the other.
func dashboardWithTwoSessions() *DashboardState {
	second := runningSession()
	second.ID = "session-2"
	dashboard := dashboardWithSession(runningSession())
	dashboard.state.Mut().Sessions.Insert(second.ID, second)
	return dashboard
}

func paneSessionIs(dashboard *DashboardState, pane PaneId, want string) bool {
	session, ok := dashboard.PaneSession(pane)
	if want == "" {
		return !ok
	}
	return ok && session == want
}

func requireSplit(t *testing.T, dashboard *DashboardState, session *string) PaneId {
	t.Helper()
	pane, ok := dashboard.SplitFocusedPane(chat.DirectionHorizontal, session)
	if !ok {
		t.Fatal("the test conversation area has room for two panes")
	}
	return pane
}

// TestLifecycleUpdatesLeaveTheUsersPaneSelectionFocusAndDraftUnchanged:
// lifecycle updates can complete after focus moved to another pane. The old
// pane keeps its session and draft; completion only changes readiness.
// Hard-won: 4d523f1: launch completion replaced the user’s newer selection and focus.
func TestLifecycleUpdatesLeaveTheUsersPaneSelectionFocusAndDraftUnchanged(t *testing.T) {
	for _, kind := range []SessionOperationKind{SessionOperationKindLaunching, SessionOperationKindMoving} {
		dashboard := dashboardWithTwoSessions()
		dashboard.SetCurrentSession(optionalText("session-1"))
		operated := dashboard.FocusedPane()
		focused := requireSplit(t, dashboard, optionalText("session-2"))
		dashboard.FocusSessions()
		dashboard.SeedStandbyPrompt("session-1", "draft for the operated session")
		dashboard.BeginSessionOperation("session-1", kind, nil)
		// The runtime may temporarily omit the operated session before its
		// completed record arrives. Its operation owns the placeholder.
		refreshed := *dashboard.state.Get()
		session, _ := refreshed.Sessions.Remove("session-1")
		dashboard.SetState(refreshed)
		refreshed.Sessions.Insert(session.ID, session)
		dashboard.SetState(refreshed)
		dashboard.FinishSessionOperation("session-1")
		if dashboard.FocusedPane() != focused {
			t.Fatalf("%v: focused pane moved", kind)
		}
		if !selectedSessionIs(dashboard, "session-2") {
			t.Fatalf("%v: selection moved", kind)
		}
		if !paneSessionIs(dashboard, operated, "session-1") {
			t.Fatalf("%v: the operated pane lost its session", kind)
		}
		if dashboard.Focus() != FocusSessions {
			t.Fatalf("%v: focus %v", kind, dashboard.Focus())
		}
		if draft, ok := dashboard.TakeStandbyPromptDraft("session-1"); !ok || draft != "draft for the operated session" {
			t.Fatalf("%v: draft %q", kind, draft)
		}
	}
}

// TestASuspendedSessionIsNotAttachedTo: R4-11: after a suspend, the
// dashboard reported "Could not open Session claude-b: Session opening did
// not respond within 15 seconds" although nothing asked to open it. The pane
// still named the suspended session, and the end of its lifecycle re-armed an
// attach to a session with no worker.
// Hard-won: 0d95dcf: suspending a pinned session triggered a needless 15-second attach.
func TestASuspendedSessionIsNotAttachedTo(t *testing.T) {
	dashboard := dashboardWithTwoSessions()
	if !dashboard.PaneSessionCanAttach("session-1") {
		t.Fatal("a running session can attach")
	}
	mutableSession(dashboard, "session-1").State = core.SessionStateStopped
	if dashboard.PaneSessionCanAttach("session-1") {
		t.Fatal("a stopped session cannot attach")
	}
}

func TestSettingTheCurrentSessionWritesOnlyTheFocusedPane(t *testing.T) {
	dashboard := dashboardWithTwoSessions()
	dashboard.SetCurrentSession(optionalText("session-1"))
	first := dashboard.FocusedPane()
	second := requireSplit(t, dashboard, optionalText("session-2"))

	dashboard.SetCurrentSession(optionalText("session-1"))

	if dashboard.FocusedPane() != second {
		t.Fatal("the split has focus")
	}
	if !paneSessionIs(dashboard, second, "session-1") {
		t.Fatal("the focused pane shows session-1")
	}
	// A session belongs to one pane, so showing it here took it out of the
	// pane it was in; no other pane's entry is touched.
	if !paneSessionIs(dashboard, first, "") {
		t.Fatal("the first pane is empty")
	}
	dashboard.SetCurrentSession(optionalText("session-2"))
	if !paneSessionIs(dashboard, second, "session-2") || !paneSessionIs(dashboard, first, "") {
		t.Fatal("only the focused pane changes")
	}
	dashboard.SetCurrentSession(nil)
	if !paneSessionIs(dashboard, second, "") {
		t.Fatal("the focused pane is emptied")
	}
}

func TestARestoredPaneWhoseSessionIsGoneOpensEmpty(t *testing.T) {
	dashboard := dashboardWithTwoSessions()
	dashboard.SetCurrentSession(optionalText("session-1"))
	first := dashboard.FocusedPane()
	second := requireSplit(t, dashboard, optionalText("session-2"))
	saved := dashboard.ConversationLayoutFor(core.DefaultWorkspaceID)

	// The second session was closed and removed while this client was away.
	restored := dashboardWithSession(runningSession())
	restored.CacheWorkspaceLayout(core.DefaultWorkspaceID, saved)

	if !paneSessionIs(restored, first, "session-1") {
		t.Fatal("the first pane keeps session-1")
	}
	if !paneSessionIs(restored, second, "") {
		t.Fatal("the second pane opens empty")
	}
}

// TestOpeningASessionShownElsewhereIsAnsweredByThePaneThatHasIt: opening a
// session already on screen is a move, not a copy: the same conversation must
// never be drawn in two panes.
func TestOpeningASessionShownElsewhereIsAnsweredByThePaneThatHasIt(t *testing.T) {
	dashboard := dashboardWithTwoSessions()
	dashboard.SetCurrentSession(optionalText("session-1"))
	first := dashboard.FocusedPane()
	second := requireSplit(t, dashboard, optionalText("session-2"))
	dashboard.FocusSessions()
	dashboard.SelectActiveSession("session-1")

	action := dashboard.DispatchCommand(CommandIdOpenSession)

	// The controller answers `Open` by moving the focus to the pane that
	// already shows it rather than attaching a second copy.
	if open, ok := action.(DashboardActionOpen); !ok || open.SessionID != "session-1" {
		t.Fatalf("%#v", action)
	}
	if pane, ok := dashboard.PaneForSession("session-1"); !ok || pane != first {
		t.Fatal("session-1 stays in the first pane")
	}
	if !paneSessionIs(dashboard, second, "session-2") {
		t.Fatal("the second pane keeps session-2")
	}
}

// TestASplitWithNoRoomIsRefusedAndChangesNothing: a split that would leave
// either half too small to use is refused, leaves the layout unchanged, and
// says why on the notice bar.
func TestASplitWithNoRoomIsRefusedAndChangesNothing(t *testing.T) {
	dashboard := dashboardWithTwoSessions()
	dashboard.SetCurrentSession(optionalText("session-1"))
	// A narrow frame leaves a conversation band too slim for two panes.
	drawn(dashboard, 80, 30)
	before := dashboard.FocusedPane()

	_, split := dashboard.SplitFocusedPane(chat.DirectionHorizontal, nil)

	if split {
		t.Fatal("the split is refused")
	}
	if dashboard.FocusedPane() != before {
		t.Fatal("focus is unchanged")
	}
	if count := dashboard.navigation.layout().PaneCount(); count != 1 {
		t.Fatalf("%d panes", count)
	}
	if notice, _ := dashboard.Notice(); notice != SplitRefusedNotice {
		t.Fatalf("notice %q", notice)
	}
}

// TestASuccessfulSplitClearsTheEarlierNoRoomNotice: launch finding D-2: the
// refusal notice describes a failed split, so a later split that succeeds
// must take it off the bar.
// Hard-won: 34e23bc: a successful split left an obsolete refusal notice visible.
func TestASuccessfulSplitClearsTheEarlierNoRoomNotice(t *testing.T) {
	dashboard := dashboardWithTwoSessions()
	dashboard.SetCurrentSession(optionalText("session-1"))
	drawn(dashboard, 80, 30)
	if _, ok := dashboard.SplitFocusedPane(chat.DirectionHorizontal, nil); ok {
		t.Fatal("the narrow split is refused")
	}
	if _, ok := dashboard.Notice(); !ok {
		t.Fatal("the refusal is noticed")
	}

	drawn(dashboard, 200, 60)
	if _, ok := dashboard.SplitFocusedPane(chat.DirectionHorizontal, nil); !ok {
		t.Fatal("a wide frame has room for two panes")
	}

	if notice, ok := dashboard.Notice(); ok {
		t.Fatalf("notice %q", notice)
	}
}

// TestASessionInstallsIntoThePaneThatAskedAndLeavesAnyOther: an attach lands
// in the pane that asked for it, whichever pane has the keyboard by the time
// it arrives, and a session is never in two panes.
func TestASessionInstallsIntoThePaneThatAskedAndLeavesAnyOther(t *testing.T) {
	dashboard := dashboardWithTwoSessions()
	dashboard.SetCurrentSession(optionalText("session-1"))
	first := dashboard.FocusedPane()
	second := requireSplit(t, dashboard, optionalText("session-2"))

	// The attach the first pane started arrives while the second has focus.
	dashboard.SetPaneSession(first, optionalText("session-1"))

	if dashboard.FocusedPane() != second {
		t.Fatal("focus stays on the second pane")
	}
	if !paneSessionIs(dashboard, first, "session-1") || !paneSessionIs(dashboard, second, "session-2") {
		t.Fatal("each pane keeps its session")
	}

	// Moving a session into another pane takes it out of the one it was in.
	dashboard.SetPaneSession(second, optionalText("session-1"))
	if !paneSessionIs(dashboard, first, "") {
		t.Fatal("the first pane let session-1 go")
	}
	if pane, ok := dashboard.PaneForSession("session-1"); !ok || pane != second {
		t.Fatal("session-1 is in the second pane")
	}
}

// TestARestoredLayoutNeverShowsOneSessionInTwoPanes: a stored arrangement
// that names one session in two panes is not a layout this surface can draw:
// the conversation would appear twice.
func TestARestoredLayoutNeverShowsOneSessionInTwoPanes(t *testing.T) {
	dashboard := dashboardWithTwoSessions()
	dashboard.SetCurrentSession(optionalText("session-1"))
	first := dashboard.FocusedPane()
	second := requireSplit(t, dashboard, optionalText("session-2"))
	saved := dashboard.ConversationLayoutFor(core.DefaultWorkspaceID)
	saved.Sessions[second.Raw()] = "session-1"

	restored := dashboardWithTwoSessions()
	restored.CacheWorkspaceLayout(core.DefaultWorkspaceID, saved)

	if !paneSessionIs(restored, first, "session-1") {
		t.Fatal("the first pane keeps session-1")
	}
	if !paneSessionIs(restored, second, "") {
		t.Fatal("the duplicate is dropped")
	}
}

// TestClosingAPaneLeavesNoPreviousPaneToReturnTo: closing a pane forgets it
// as the place to go back to, so the key never moves the keyboard into a
// pane that is gone.
func TestClosingAPaneLeavesNoPreviousPaneToReturnTo(t *testing.T) {
	dashboard := dashboardWithTwoSessions()
	dashboard.SetCurrentSession(optionalText("session-1"))
	first := dashboard.FocusedPane()
	second := requireSplit(t, dashboard, optionalText("session-2"))
	dashboard.FocusPane(first)
	dashboard.FocusPane(second)
	dashboard.FocusPrompt()

	dashboard.ClosePane(first)

	requireDashboardAction(t, chord(dashboard, CommandIdFocusLastPane), DashboardActionNone{})
	if notice, _ := dashboard.Notice(); notice != "No previous pane" {
		t.Fatalf("notice %q", notice)
	}
	if dashboard.FocusedPane() != second {
		t.Fatal("focus stays on the second pane")
	}
}

// dashboardWithAttentionMix is three live sessions in the default workspace
// and one in `other`: `asks` is waiting on a question, `done` has an unread
// answer, `quiet` is idle and read, and `remote` (in `other`) is also waiting
// on a question.
func dashboardWithAttentionMix() *DashboardState {
	state := core.NewStateState()
	for _, fixture := range []struct{ id, workspace, created string }{
		{"quiet", "default", "2026-08-01T00:00:00Z"},
		{"asks", "default", "2026-08-02T00:00:00Z"},
		{"done", "default", "2026-08-03T00:00:00Z"},
		{"remote", "other", "2026-08-04T00:00:00Z"},
	} {
		session := runningSession()
		session.ID = fixture.id
		session.WorkspaceID = fixture.workspace
		session.CreatedAt = fixture.created
		session.ProjectDirectory = optionalText("/projects/" + fixture.id)
		state.Sessions.Insert(session.ID, session)
	}
	dashboard := NewDashboardState(config(), state, map[string]client.ProfileQuota{})
	attention := core.NewSnapshotMap[string, core.SessionAttention]()
	attention.Insert("asks", core.SessionAttentionNeedsInput)
	attention.Insert("remote", core.SessionAttentionNeedsInput)
	attention.Insert("done", core.SessionAttentionUnread)
	attention.Insert("quiet", core.SessionAttentionIdle)
	dashboard.SetSessionAttention(attention)
	dashboard.SetWorkspaceNames(map[string]string{"default": "Default", "other": "Other"})
	for _, id := range []string{"asks", "remote"} {
		mutableSessionDetail(dashboard, id).pendingElicitations = []core.ElicitationRequest{question(id)}
	}
	mutableSessionDetail(dashboard, "done").unreadAgentMessages = 1
	publishFixtureVerdicts(dashboard)
	dashboard.SetUnreadFrontiers(frontiers(receipt("done", 4)))
	return dashboard
}

func appendAttentionState(output *strings.Builder, label string, dashboard *DashboardState, width, height int) []string {
	lines := drawn(dashboard, width, height)
	if output.Len() > 0 {
		output.WriteString("\n")
	}
	fmt.Fprintf(output, "=== %s (%dx%d) ===\n", label, width, height)
	output.WriteString(strings.Join(lines, "\n"))
	output.WriteString("\n")
	return lines
}

// attentionLevelName is AttentionLevel's Rust Debug name.
func attentionLevelName(level AttentionLevel) string {
	names := [...]string{"Inactive", "Idle", "Working", "Unread", "Important", "Waiting", "Unreachable", "Failed"}
	if level < 0 || int(level) >= len(names) {
		return fmt.Sprintf("AttentionLevel(%d)", int(level))
	}
	return names[level]
}

// attentionSummaryDebug is Rust's Debug of `Option<(AttentionLevel, usize)>`.
func attentionSummaryDebug(level AttentionLevel, count int, ok bool) string {
	if !ok {
		return "None"
	}
	return fmt.Sprintf("Some((%s, %d))", attentionLevelName(level), count)
}

// optionDebug is Rust's Debug of an `Option<&str>`.
func optionDebug(value string, ok bool) string {
	if !ok {
		return "None"
	}
	return fmt.Sprintf("Some(%s)", rustDebug(value))
}

// rustDebug renders v as Rust's derived Debug does for the values these
// tests print: unit and struct enum variants of the dashboard's sealed
// interfaces (the Go type name less its interface prefix), strings, string
// lists, booleans, numbers, and attention levels.
func rustDebug(v any) string {
	switch v := v.(type) {
	case nil:
		return "None"
	case AttentionLevel:
		return attentionLevelName(v)
	case CommandId:
		return v.String()
	case string:
		return fmt.Sprintf("%q", v)
	case []string:
		parts := make([]string, len(v))
		for index, item := range v {
			parts[index] = rustDebug(item)
		}
		return "[" + strings.Join(parts, ", ") + "]"
	case bool, int, int64, uint64, uint32:
		return fmt.Sprint(v)
	}
	value := reflect.ValueOf(v)
	if value.Kind() == reflect.Pointer {
		if value.IsNil() {
			return "None"
		}
		return fmt.Sprintf("Some(%s)", rustDebug(value.Elem().Interface()))
	}
	if value.Kind() == reflect.Slice {
		parts := make([]string, value.Len())
		for index := range value.Len() {
			parts[index] = rustDebug(value.Index(index).Interface())
		}
		return "[" + strings.Join(parts, ", ") + "]"
	}
	if value.Kind() != reflect.Struct {
		return fmt.Sprintf("%#v", v)
	}
	name := value.Type().Name()
	for _, prefix := range []string{"DashboardAction"} {
		name = strings.TrimPrefix(name, prefix)
	}
	if value.NumField() == 0 {
		return name
	}
	fields := make([]string, 0, value.NumField())
	for index := range value.NumField() {
		field := value.Type().Field(index)
		fields = append(fields, fmt.Sprintf("%s: %s", snakeCase(field.Name), rustDebug(value.Field(index).Interface())))
	}
	return fmt.Sprintf("%s { %s }", name, strings.Join(fields, ", "))
}

// snakeCase turns a Go field name into its Rust field name (SessionID is
// session_id).
func snakeCase(name string) string {
	name = strings.ReplaceAll(name, "ID", "Id")
	var out strings.Builder
	for index, character := range name {
		if character >= 'A' && character <= 'Z' {
			if index > 0 {
				out.WriteByte('_')
			}
			character += 'a' - 'A'
		}
		out.WriteRune(character)
	}
	return out.String()
}

// setDebug is Rust's Debug of a BTreeSet<String>.
func setDebug(set map[string]struct{}) string {
	keys := make([]string, 0, len(set))
	for key := range set {
		keys = append(keys, rustDebug(key))
	}
	slices.Sort(keys)
	return "{" + strings.Join(keys, ", ") + "}"
}

func attentionValues(dashboard *DashboardState) string {
	var levels []string
	for _, id := range []string{"asks", "remote", "done", "quiet", "missing"} {
		levels = append(levels, fmt.Sprintf("%s=%s", id, attentionLevelName(dashboard.AttentionLevel(id))))
	}
	var queue []string
	for _, entry := range dashboard.AttentionQueue() {
		queue = append(queue, fmt.Sprintf("%s:%s", entry.SessionID, attentionLevelName(entry.Level)))
	}
	return fmt.Sprintf("attention levels: %s; queue: [%s]", strings.Join(levels, ", "), strings.Join(queue, ", "))
}

func workspaceTabArea(t *testing.T, dashboard *DashboardState, workspaceID string) uv.Rectangle {
	t.Helper()
	for _, tab := range dashboard.workspaceTabAreas {
		if tab.First == workspaceID {
			return tab.Second
		}
	}
	t.Fatalf("workspace tab %q", workspaceID)
	return uv.Rectangle{}
}

func cellSymbol(buffer *uv.Buffer, x, y int) string {
	cell := buffer.CellAt(x, y)
	if cell == nil || cell.Content == "" {
		return " "
	}
	return cell.Content
}

func renderedWorkspaceBadge(t *testing.T, dashboard *DashboardState, workspaceID string, width, height int) string {
	t.Helper()
	terminal := newRatatuiTestTerminal(width, height)
	terminal.draw(func(frame *chat.Frame) { renderDashboardForTest(frame, dashboard) })
	area := workspaceTabArea(t, dashboard, workspaceID)
	marker := terminal.buffer.CellAt(area.Max.X-3, area.Min.Y)
	markerFg := chat.CellStyle(marker).Fg
	return fmt.Sprintf(
		"workspace badge cells: %s%s; marker fg=%#v; failure color=%v",
		cellSymbol(terminal.buffer, area.Max.X-3, area.Min.Y),
		cellSymbol(terminal.buffer, area.Max.X-2, area.Min.Y),
		markerFg,
		markerFg == chat.ThemePalette().SessionError,
	)
}

// TestSessionOrderAndAttentionAreDerivedOncePerChangeAndFollowIt: the
// Sessions order and the attention levels are derived once per change of
// what they come from, however often a frame reads them, and each change is
// reflected in the next read.
func TestSessionOrderAndAttentionAreDerivedOncePerChangeAndFollowIt(t *testing.T) {
	dashboard := dashboardWithAttentionMix()
	dashboard.config.Mut().Advanced.SessionOrder = core.SessionOrderPriority
	dashboard.SetActiveWorkspace(workspaceID("default"))
	dashboard.SelectActiveSession("done")
	drawn(dashboard, 160, 40)
	requireIDs(t, orderedSessionIDs(dashboard), "asks", "done", "quiet")

	settledFacts, settledOrder := dashboard.sessionViewDerivations()
	for range 3 {
		drawn(dashboard, 160, 40)
		dashboard.NotificationEvents()
	}
	if facts, order := dashboard.sessionViewDerivations(); facts != settledFacts || order != settledOrder {
		t.Fatalf("frames with nothing changed derive nothing: (%d, %d) after (%d, %d)", facts, order, settledFacts, settledOrder)
	}

	// A question makes `quiet` waiting too. It was created first, so it now
	// leads the sessions at that level.
	mutableSessionDetail(dashboard, "quiet").pendingElicitations = []core.ElicitationRequest{question("quiet")}
	serveAttention(dashboard, []string{"quiet"}, core.SessionAttentionNeedsInput)
	drawn(dashboard, 160, 40)
	drawn(dashboard, 160, 40)
	requireAttention(t, dashboard, "quiet", AttentionLevelWaiting)
	requireIDs(t, orderedSessionIDs(dashboard), "quiet", "asks", "done")
	if facts, order := dashboard.sessionViewDerivations(); facts != settledFacts+1 || order != settledOrder+1 {
		t.Fatalf("one change, one derivation: (%d, %d) after (%d, %d)", facts, order, settledFacts, settledOrder)
	}

	// A filter narrows the list to what matches, and says how much it hides.
	dashboard.FocusSessions()
	dashboard.HandleKey(key('/'))
	typeKeys(dashboard, "done")
	dashboard.HandleKey(key(tea.KeyEnter))
	requireIDs(t, orderedSessionIDs(dashboard), "done")
	if hidden := dashboard.sessionsHiddenCount(); hidden != 2 {
		t.Fatalf("hidden %d", hidden)
	}

	// Moving the selection to a hidden session keeps that session listed.
	dashboard.SelectActiveSession("asks")
	requireIDs(t, orderedSessionIDs(dashboard), "asks", "done")
	if hidden := dashboard.sessionsHiddenCount(); hidden != 1 {
		t.Fatalf("hidden %d", hidden)
	}

	dashboard.sessionsFilter.Set(nil)
	requireIDs(t, orderedSessionIDs(dashboard), "quiet", "asks", "done")
	if hidden := dashboard.sessionsHiddenCount(); hidden != 0 {
		t.Fatalf("hidden %d", hidden)
	}
}

func TestAFailedStopIsAFailureForTheQueueAsWellAsTheRow(t *testing.T) {
	dashboard := dashboardWithAttentionMix()
	session := mutableSession(dashboard, "quiet")
	session.State = core.SessionStateClosing
	session.LastError = optionalText("the worker would not stop")

	serveAttention(dashboard, []string{"quiet"}, core.SessionAttentionFailed)
	requireAttention(t, dashboard, "quiet", AttentionLevelFailed)
	if queue := dashboard.AttentionQueue(); len(queue) == 0 || queue[0].SessionID != "quiet" {
		t.Fatalf("queue %#v", queue)
	}
}

func requireWorkspaceSummary(t *testing.T, dashboard *DashboardState, workspaceID string, want string, message ...any) {
	t.Helper()
	if got := attentionSummaryDebug(dashboard.workspaceAttentionSummary(workspaceID)); got != want {
		t.Fatalf("%s: workspace summary %s, want %s", fmt.Sprint(message...), got, want)
	}
}

// Hard-won: #1517: missing daemon attention hid the unread sessions after a failure was viewed.
func TestViewingOneFailureRevealsTheFiveUnreadSessionsWithoutRelabelingThem(t *testing.T) {
	failed := runningSession()
	failed.ID = "failed"
	failed.State = core.SessionStateError
	failed.LastError = optionalText("worker stopped unexpectedly")
	dashboard := dashboardWithSession(failed)
	state := *dashboard.state.Get()
	for index := range 5 {
		session := runningSession()
		session.ID = fmt.Sprintf("done-%d", index)
		state.Sessions.Insert(session.ID, session)
	}
	dashboard.SetState(state)
	for index := range 5 {
		id := fmt.Sprintf("done-%d", index)
		detail := mutableSessionDetail(dashboard, id)
		detail.agentMessageLatestContentOrdinals = []uint64{1}
		detail.unreadAgentMessages = 1
		serveAttention(dashboard, []string{id}, core.SessionAttentionUnread)
	}
	serveAttention(dashboard, []string{"failed"}, core.SessionAttentionFailed)
	serveAttention(dashboard, []string{"done-0", "done-1", "done-2", "done-3", "done-4"}, core.SessionAttentionUnread)
	dashboard.SelectActiveSession("failed")
	requireWorkspaceSummary(t, dashboard, "default", "Some((Failed, 1))")
	drawn(dashboard, 40, 10)
	dashboard.AcknowledgeRender()
	requireWorkspaceSummary(t, dashboard, "default", "Some((Failed, 1))", "a terminal-too-small warning does not show the failure")

	identity := core.FailureIdentityOf(dashboard.state.Get().Sessions.GetShared("failed"))
	if identity == nil {
		t.Fatal("the failed session has a failure identity")
	}
	dashboard.SetNotificationDismissals(dismissals("failed", core.NotificationDismissalFailure{Value: *identity}))
	screen := strings.Join(drawn(dashboard, 140, 40), "\n")
	if !strings.Contains(screen, "worker stopped unexpectedly") {
		t.Fatal(screen)
	}
	dashboard.AcknowledgeRender()
	actions := dashboard.TakeDrawnFailureDismissals()
	if len(actions) != 1 {
		t.Fatalf("actions %#v", actions)
	}
	requireDashboardAction(t, actions[0], DashboardActionDismissNotification{
		SessionID: "failed",
		Target:    core.NotificationDismissalFailure{Value: *identity},
	})
	requireWorkspaceSummary(t, dashboard, "default", "Some((Failed, 1))", "only the daemon changes the alert")
	serveAttention(dashboard, []string{"failed"}, core.SessionAttentionInactive)
	dashboard.SetNotificationDismissals(core.NewSnapshotMap[string, core.NotificationDismissal]())
	requireWorkspaceSummary(t, dashboard, "default", "Some((Unread, 5))")
	serveAttention(dashboard, []string{"failed"}, core.SessionAttentionFailed)
	requireWorkspaceSummary(t, dashboard, "default", "Some((Failed, 1))")
}

// Hard-won: bed9102: terminal failures inflated badges and queued an unopenable session.
// Hard-won: 0db309adc: prefix+o reopened the resume dialog on the same stopped failure, so traversal never advanced.
func TestAStoppedNotificationIsARowAndAttentionAdvances(t *testing.T) {
	dashboard := dashboardWithAttentionMix()
	dashboard.SetActiveWorkspace(workspaceID("default"))
	requireWorkspaceSummary(t, dashboard, "default", "Some((Waiting, 1))")

	for _, state := range []core.SessionState{
		core.SessionStateStopped,
		core.SessionStateDestroyedWithDataLoss,
		core.SessionStateLost,
	} {
		record := mutableSession(dashboard, "quiet")
		record.State = state
		record.LastError = optionalText("resume failed")
		failure := core.FailureIdentityOf(record)
		if failure == nil {
			t.Fatalf("%v: a failure identity", state)
		}
		verdict := dashboard.verdict("quiet")
		verdict.LiveDashboard = false
		verdict.Resume = client.SessionOfferNoRecoveryCopy
		if state == core.SessionStateStopped {
			verdict.Resume = client.SessionOfferReady
		}
		dashboard.setSessionVerdict("quiet", verdict)
		serveAttention(dashboard, []string{"quiet"}, core.SessionAttentionFailed)
		dashboard.SetNotificationDismissals(dismissals("quiet", core.NotificationDismissalFailure{Value: *failure}))
		if !slices.Contains(orderedSessionIDs(dashboard), "quiet") {
			t.Fatalf("%v is listed while its failure needs a person", state)
		}
		requireWorkspaceSummary(t, dashboard, "default", "Some((Failed, 1))", state, " needs a person")

		// The first press selects the stopped row without opening anything;
		// the second moves on to the question instead of returning to it.
		dashboard.SelectActiveSession("done")
		if action := dashboard.stepAttention(1); reflectSnapshot(action) != reflectSnapshot(DashboardAction(DashboardActionNone{})) {
			t.Fatalf("%v: %#v", state, action)
		}
		if !selectedSessionIs(dashboard, "quiet") {
			t.Fatalf("%v: quiet is selected", state)
		}
		if dashboard.focus != FocusSessions {
			t.Fatalf("%v: focus %v", state, dashboard.focus)
		}
		if !isDashboardMode(dashboard) {
			t.Fatalf("%v: mode %#v", state, dashboard.mode)
		}
		if open, ok := dashboard.stepAttention(1).(DashboardActionOpen); !ok || open.SessionID != "asks" {
			t.Fatalf("%v: the second press opens the question", state)
		}

		// Enter resumes a stopped record and refuses one with nothing left.
		dashboard.SelectActiveSession("quiet")
		dashboard.FocusSessions()
		dashboard.HandleKey(key(tea.KeyEnter))
		if state == core.SessionStateStopped {
			if _, ok := dashboard.mode.(ModeResume); !ok {
				t.Fatalf("%v: mode %#v", state, dashboard.mode)
			}
			dashboard.mode = ModeDashboard{}
		} else {
			if !isDashboardMode(dashboard) {
				t.Fatalf("%v: mode %#v", state, dashboard.mode)
			}
			notice, _ := dashboard.notices.Current()
			if !strings.Contains(notice, "Use Destroy") {
				t.Fatalf("%v: %s", state, notice)
			}
		}

		dashboard.FocusSessions()
		action := dashboard.DispatchCommand(CommandIdDismissNotification)
		if dismiss, ok := action.(DashboardActionDismissNotification); !ok || dismiss.SessionID != "quiet" {
			t.Fatalf("%v: %#v", state, action)
		}
		dashboard.NotificationDismissalFinished("quiet", core.NotificationDismissalFailure{Value: *failure}, 0, nil)
		dashboard.SetNotificationDismissals(core.NewSnapshotMap[string, core.NotificationDismissal]())
	}

	// An error state still has a row, so it still counts.
	mutableSession(dashboard, "quiet").State = core.SessionStateError
	requireWorkspaceSummary(t, dashboard, "default", "Some((Failed, 1))")

	// In another workspace, reached from the Workspaces pane, the switch
	// still lands on the row with Sessions focused, and the host opens
	// nothing.
	mutableSession(dashboard, "quiet").State = core.SessionStateRunning
	serveAttention(dashboard, []string{"quiet"}, core.SessionAttentionIdle)
	mutableSession(dashboard, "remote").State = core.SessionStateStopped
	serveAttention(dashboard, []string{"remote"}, core.SessionAttentionFailed)
	dashboard.SelectActiveSession("done")
	dashboard.focus = FocusWorkspaces
	requireDashboardAction(t, dashboard.stepAttention(1), DashboardActionSelectWorkspace{WorkspaceID: "other"})
	dashboard.SetActiveWorkspace(workspaceID("other"))
	if session, ok := dashboard.TakeSessionToOpen(); ok {
		t.Fatalf("the host opens nothing, not %q", session)
	}
	// The host then releases every pane holding a suspended session, as
	// `open_chat_session_into` does after a switch. Browse keeps the row.
	for _, entry := range dashboard.PaneSessions() {
		if dashboard.PaneSessionIsSuspended(entry.SessionID) {
			dashboard.ReleaseSuspendedPane(entry.Pane, entry.SessionID)
		}
	}
	if !selectedSessionIs(dashboard, "remote") {
		t.Fatal("remote is selected")
	}
	if dashboard.focus != FocusSessions {
		t.Fatalf("focus %v", dashboard.focus)
	}
	requireDashboardAction(t, dashboard.stepAttention(1), DashboardActionSelectWorkspace{WorkspaceID: "default"},
		"the next press leaves the stopped row for the question")
}

// Hard-won: dc77f11c1: mark-all-read missed Important completions behind a question badge.
func TestMarkingAllReadClearsTheUnreadBadgeButLeavesAQuestionFlagged(t *testing.T) {
	dashboard := dashboardWithAttentionMix()
	dashboard.SetActiveWorkspace(workspaceID("default"))
	dashboard.SetUnreadFrontiers(frontiers(receipt("asks", 4), receipt("done", 4)))
	dashboard.SetNotificationDismissals(dismissals("asks", core.NotificationDismissalStack{Revision: 10}))
	var entries []core.NotificationStackEntry
	for _, item := range []struct {
		ordinal uint64
		kind    core.NotificationKind
	}{
		{5, core.NotificationKindImportant},
		{6, core.NotificationKindImportant},
		{9, core.NotificationKindNeedsInput},
	} {
		entries = append(entries, core.NotificationStackEntry{
			CompletedOrdinal: item.ordinal,
			Kind:             item.kind,
			Message:          core.EntryMessageNone{},
		})
	}
	cards := core.NewSnapshotMap[string, client.NotificationCard]()
	cards.Insert("asks", client.NotificationCard{
		Badge:     core.NotificationKindNeedsInput,
		Entries:   entries,
		Dismissal: core.NotificationDismissalStack{Revision: 10},
	})
	dashboard.SetNotificationCards(&cards)
	if summary := attentionSummaryDebug(dashboard.sessionsAttentionSummary()); summary != "Some((Waiting, 1))" {
		t.Fatalf("summary %s", summary)
	}

	markAllRead, ok := chord(dashboard, CommandIdMarkAllRead).(DashboardActionMarkAllRead)
	if !ok {
		t.Fatal("mark all read must save receipts")
	}
	receipts := map[string]uint64{}
	for _, pair := range markAllRead.Receipts {
		receipts[pair.First] = pair.Second
	}
	if !reflect.DeepEqual(receipts, map[string]uint64{"asks": 6, "done": 4}) {
		t.Fatalf("receipts %v", receipts)
	}
	if viewed := viewedThrough(dashboard, "asks"); viewed != 6 {
		t.Fatalf("asks viewed through %d", viewed)
	}
	if dashboard.AttentionLevel("asks") != AttentionLevelWaiting {
		t.Fatal("a pending question survives mark all read")
	}
	if summary := attentionSummaryDebug(dashboard.sessionsAttentionSummary()); summary != "Some((Waiting, 1))" {
		t.Fatalf("summary %s", summary)
	}
}

// Hard-won: 1b2ef72: short names and wide graphemes clipped the workspace badge.
func TestShortWorkspaceNamesKeepTheirAttentionBadges(t *testing.T) {
	for _, name := range []string{"M", "MJ", "界", "é"} {
		dashboard := dashboardWithAttentionMix()
		dashboard.SetWorkspaceNames(map[string]string{"default": name, "other": "Other"})
		dashboard.SetActiveWorkspace(workspaceID("default"))
		terminal := newRatatuiTestTerminal(120, 40)
		terminal.draw(func(frame *chat.Frame) { renderDashboardForTest(frame, dashboard) })
		area := workspaceTabArea(t, dashboard, "default")
		if symbol := cellSymbol(terminal.buffer, area.Max.X-3, area.Min.Y); symbol != "?" {
			t.Fatalf("%s: marker %q", name, symbol)
		}
		if symbol := cellSymbol(terminal.buffer, area.Max.X-2, area.Min.Y); symbol != "1" {
			t.Fatalf("%s: count %q", name, symbol)
		}
	}
}

func TestGoldenAttentionNavigation(t *testing.T) {
	var output strings.Builder

	ranked := dashboardWithAttentionMix()
	ranked.SetActiveWorkspace(workspaceID("default"))
	appendAttentionState(&output, "question, unread, idle, and absent levels", ranked, 140, 40)
	fmt.Fprintln(&output, attentionValues(ranked))

	stopped := dashboardWithAttentionMix()
	stopped.SetActiveWorkspace(workspaceID("default"))
	record := mutableSession(stopped, "quiet")
	record.State = core.SessionStateStopped
	record.LastError = optionalText("resume failed: target unavailable")
	serveAttention(stopped, []string{"quiet"}, core.SessionAttentionFailed)
	stopped.SelectActiveSession("quiet")
	appendAttentionState(&output, "stopped failure remains a row", stopped, 140, 40)

	urgent := dashboardWithAttentionMix()
	urgent.SetActiveWorkspace(workspaceID("default"))
	urgent.SetSessionReviews([]client.RuntimeReviewView{{
		SessionID: "asks",
		Phase: core.TurnReviewPhaseVerdict{
			Verdict: core.ReviewVerdictFailed{Reason: "the reviewer never answered"},
		},
		Status: "the review failed",
	}})
	serveAttention(urgent, []string{"remote"}, core.SessionAttentionUnreachable)
	urgent.SetSessionConnectivity("remote", false)
	serveAttention(urgent, []string{"asks"}, core.SessionAttentionFailed)

	appendAttentionState(&output, "failure, unreachable worker, and question", urgent, 140, 40)
	fmt.Fprintln(&output, attentionValues(urgent))
	fmt.Fprintf(&output,
		"rank relation: Failed>Unreachable=%v, Unreachable>Waiting=%v\n",
		AttentionLevelFailed > AttentionLevelUnreachable,
		AttentionLevelUnreachable > AttentionLevelWaiting,
	)

	badges := dashboardWithAttentionMix()
	badges.SetActiveWorkspace(workspaceID("default"))
	appendAttentionState(&output, "waiting badge counts only its level", badges, 120, 40)
	fmt.Fprintf(&output, "workspace summary: %s\n", attentionSummaryDebug(badges.workspaceAttentionSummary("default")))
	fmt.Fprintln(&output, renderedWorkspaceBadge(t, badges, "default", 120, 40))
	badges.SetSessionConnectivity("done", false)
	serveAttention(badges, []string{"done"}, core.SessionAttentionUnreachable)

	appendAttentionState(&output, "unreachable badge replaces unread", badges, 120, 40)
	fmt.Fprintf(&output, "workspace summary: %s\n", attentionSummaryDebug(badges.workspaceAttentionSummary("default")))
	fmt.Fprintln(&output, renderedWorkspaceBadge(t, badges, "default", 120, 40))
	mutableSession(badges, "quiet").State = core.SessionStateError
	serveAttention(badges, []string{"quiet"}, core.SessionAttentionFailed)

	appendAttentionState(&output, "failure badge replaces lower levels", badges, 120, 40)
	fmt.Fprintf(&output, "workspace summary: %s\n", attentionSummaryDebug(badges.workspaceAttentionSummary("default")))
	fmt.Fprintln(&output, renderedWorkspaceBadge(t, badges, "default", 120, 40))
	quiet := dashboardWithSession(runningSession())
	appendAttentionState(&output, "quiet workspace has no attention badge", quiet, 120, 40)
	fmt.Fprintf(&output, "workspace summary: %s; badge: absent\n", attentionSummaryDebug(quiet.workspaceAttentionSummary("default")))

	next := dashboardWithAttentionMix()
	next.SetActiveWorkspace(workspaceID("default"))
	mutableSessionDetail(next, "asks").lastActivityAtMs = millis(20)
	mutableSessionDetail(next, "remote").lastActivityAtMs = millis(10)
	next.SelectActiveSession("quiet")
	action := chord(next, CommandIdNextAttention)
	appendAttentionState(&output, "next attention opens the first waiting session", next, 140, 40)
	fmt.Fprintf(&output, "action: %s; selected: %s; prompt focused: %v\n",
		rustDebug(action), optionDebug(next.SelectedSessionID()), next.PromptHasFocus())
	action = chord(next, CommandIdNextAttention)
	appendAttentionState(&output, "next attention reaches another workspace", next, 140, 40)
	fmt.Fprintf(&output, "action: %s\n", rustDebug(action))
	next.SetActiveWorkspace(workspaceID("other"))
	appendAttentionState(&output, "other workspace selection is applied", next, 140, 40)
	fmt.Fprintf(&output, "selected: %s\n", optionDebug(next.SelectedSessionID()))
	action = chord(next, CommandIdPreviousAttention)
	appendAttentionState(&output, "previous attention returns to the default workspace", next, 140, 40)
	fmt.Fprintf(&output, "action: %s\n", rustDebug(action))
	next.SetActiveWorkspace(workspaceID("default"))
	appendAttentionState(&output, "default workspace selection is restored", next, 140, 40)
	fmt.Fprintf(&output, "selected: %s\n", optionDebug(next.SelectedSessionID()))
	next.SelectActiveSession("done")
	action = chord(next, CommandIdNextAttention)
	appendAttentionState(&output, "next attention wraps to the first waiting session", next, 140, 40)
	fmt.Fprintf(&output, "action: %s; selected: %s\n", rustDebug(action), optionDebug(next.SelectedSessionID()))

	tied := dashboardWithAttentionMix()
	serveAttention(tied, []string{"done"}, core.SessionAttentionNeedsInput)
	tied.SetActiveWorkspace(workspaceID("default"))
	mutableSessionDetail(tied, "done").pendingElicitations = []core.ElicitationRequest{question("done")}
	for _, activity := range []struct {
		id string
		at uint64
	}{{"asks", 10}, {"done", 20}, {"remote", 30}} {
		at := activity.at
		mutableSessionDetail(tied, activity.id).lastActivityAtMs = &at
	}
	tied.SelectActiveSession("quiet")
	action = chord(tied, CommandIdNextAttention)
	appendAttentionState(&output, "equal urgency prefers the current workspace then newest activity", tied, 140, 40)
	fmt.Fprintln(&output, attentionValues(tied))
	fmt.Fprintf(&output, "action: %s; selected: %s\n", rustDebug(action), optionDebug(tied.SelectedSessionID()))
	tied.SetActiveWorkspace(workspaceID("other"))
	fmt.Fprintf(&output, "after switching workspace: %s\n", attentionValues(tied))
	tied.SetActiveWorkspace(workspaceID("default"))
	mutableSession(tied, "remote").State = core.SessionStateError
	serveAttention(tied, []string{"remote"}, core.SessionAttentionFailed)

	tied.SelectActiveSession("quiet")
	action = chord(tied, CommandIdNextAttention)
	fmt.Fprintf(&output, "higher urgency elsewhere: %s; action: %s\n", attentionValues(tied), rustDebug(action))

	cycling := dashboardWithAttentionMix()
	mutableSession(cycling, "done").WorkspaceID = "third"
	mutableSessionDetail(cycling, "done").pendingElicitations = []core.ElicitationRequest{question("done")}
	serveAttention(cycling, []string{"done"}, core.SessionAttentionNeedsInput)
	cycling.SetActiveWorkspace(workspaceID("default"))
	cycling.SelectActiveSession("quiet")
	for _, command := range []CommandId{CommandIdNextAttention, CommandIdPreviousAttention} {
		var visited []string
		for range 6 {
			// Hard-won: c7d63771d: changing workspace preference during navigation stranded notifications.
			action := chord(cycling, command)
			if selectWorkspace, ok := action.(DashboardActionSelectWorkspace); ok {
				cycling.SetActiveWorkspace(workspaceID(selectWorkspace.WorkspaceID))
				cycling.TakeSessionToOpen()
			}
			selected, ok := cycling.SelectedSessionID()
			if !ok {
				t.Fatalf("%v: nothing selected", command)
			}
			visited = append(visited, selected)
		}
		fmt.Fprintf(&output, "three-workspace %s: %s\n", command, rustDebug(visited))
	}
	cycling.SetActiveWorkspace(workspaceID("third"))
	cycling.SelectActiveSession("done")
	action = chord(cycling, CommandIdPreviousAttention)
	fmt.Fprintf(&output, "manual selection resets workspace preference: %s\n", rustDebug(action))

	for _, interruption := range []string{"session selection", "workspace switch", "resolved notification"} {
		interrupted := dashboardWithAttentionMix()
		mutableSession(interrupted, "done").WorkspaceID = "third"
		mutableSession(interrupted, "quiet").WorkspaceID = "other"
		for _, activity := range []struct {
			id string
			at uint64
		}{{"asks", 40}, {"remote", 30}, {"done", 20}, {"quiet", 10}} {
			detail := mutableSessionDetail(interrupted, activity.id)
			detail.pendingElicitations = []core.ElicitationRequest{question(activity.id)}
			at := activity.at
			detail.lastActivityAtMs = &at
		}
		interrupted.SetActiveWorkspace(workspaceID("default"))
		serveAttention(interrupted, []string{"asks", "remote", "done", "quiet"}, core.SessionAttentionNeedsInput)

		interrupted.SelectActiveSession("asks")
		requireDashboardAction(t, chord(interrupted, CommandIdNextAttention), DashboardActionSelectWorkspace{WorkspaceID: "other"})
		interrupted.SetActiveWorkspace(workspaceID("other"))
		interrupted.TakeSessionToOpen()
		switch interruption {
		case "session selection":
			interrupted.SelectActiveSession("remote")
		case "workspace switch":
			interrupted.SetActiveWorkspace(workspaceID("default"))
			interrupted.SetActiveWorkspace(workspaceID("other"))
		case "resolved notification":
			mutableSessionDetail(interrupted, "remote").pendingElicitations = nil
		}
		if interruption == "resolved notification" {
			serveAttention(interrupted, []string{"remote"}, core.SessionAttentionIdle)
		}
		// Hard-won: 01e5b6981: restoring the same selection retained an earlier workspace's preference.
		// Hard-won: 406dfc203: resolving a selected notification retained the earlier preference.
		action := chord(interrupted, CommandIdNextAttention)
		fmt.Fprintf(&output, "workspace preference after %s: %s\n", interruption, rustDebug(action))
	}

	folded := dashboardWithAttentionMix()
	folded.SetActiveWorkspace(workspaceID("default"))
	beforeFold := appendAttentionState(&output, "project rows before folding", folded, 140, 40)
	asksHeading := point(beforeFold, "asks")
	folded.HandleMouse(mouseAt(leftButtonDown, asksHeading))
	folded.HandleMouse(mouseAt(leftButtonUp, asksHeading))
	appendAttentionState(&output, "waiting project folded", folded, 140, 40)
	mutableSessionDetail(folded, "asks").lastActivityAtMs = millis(20)
	folded.SelectActiveSession("quiet")
	action = chord(folded, CommandIdNextAttention)
	appendAttentionState(&output, "next attention unfolds and opens the waiting project", folded, 140, 40)
	fmt.Fprintf(&output, "action: %s; folded projects: %s\n", rustDebug(action), setDebug(*folded.collapsedProjectKeys.Get()))
	for _, id := range []string{"asks", "remote"} {
		mutableSessionDetail(folded, id).pendingElicitations = nil
	}
	mutableSessionDetail(folded, "done").unreadAgentMessages = 0
	serveAttention(folded, []string{"asks", "remote", "done"}, core.SessionAttentionIdle)
	action = chord(folded, CommandIdNextAttention)
	appendAttentionState(&output, "empty attention queue reports its notice", folded, 140, 40)
	fmt.Fprintf(&output, "action: %s; notice: %s\n", rustDebug(action), optionDebug(folded.notices.Current()))

	footer := dashboardWithAttentionMix()
	footer.SetActiveWorkspace(workspaceID("default"))
	lines := appendAttentionState(&output, "footer advertises next attention while waiting", footer, 200, 40)
	fmt.Fprintf(&output, "footer: %s\n", lines[len(lines)-1])
	quietFooter := dashboardWithSession(runningSession())
	lines = appendAttentionState(&output, "footer omits next attention while quiet", quietFooter, 200, 40)
	fmt.Fprintf(&output, "footer: %s\n", lines[len(lines)-1])

	tabs := dashboardWithAttentionMix()
	tabs.SetActiveWorkspace(workspaceID("default"))
	first := appendAttentionState(&output, "workspace tabs show their urgent badges", tabs, 120, 40)
	tabRow := ""
	for _, line := range first {
		if strings.Contains(line, "Default") && strings.Contains(line, "Other") {
			tabRow = line
			break
		}
	}
	if tabRow == "" {
		t.Fatal("workspace tabs")
	}
	fmt.Fprintf(&output, "workspace tabs: %s\n", tabRow)
	doneHeading := point(first, "done")
	tabs.HandleMouse(mouseAt(leftButtonDown, doneHeading))
	tabs.HandleMouse(mouseAt(leftButtonUp, doneHeading))
	foldedLines := appendAttentionState(&output, "folded project heading carries unread badge", tabs, 120, 40)
	heading := ""
	for _, line := range foldedLines {
		if strings.Contains(line, "done ✓1") {
			heading = line
			break
		}
	}
	if heading == "" {
		t.Fatal("unread badge on folded heading")
	}
	fmt.Fprintf(&output, "folded heading: %s\n", heading)

	dropdown := dashboardWithAttentionMix()
	dropdown.SetActiveWorkspace(workspaceID("default"))
	dropdown.SetPaneSize(SupportPaneSessions, core.PaneSizeMinimized)
	drawn(dropdown, 120, 40)
	chord(dropdown, CommandIdFocusWorkspaces)
	dropdown.HandleKey(key(tea.KeyEnter))
	// Hard-won: f82d5aa4b: the tiny workspace dropdown hid attention in inactive workspaces.
	lines = appendAttentionState(&output, "tiny workspace dropdown shows current and inactive attention", dropdown, 120, 40)
	if !anyLineContains(lines, "?1 Default (current)") || !anyLineContains(lines, "?1 Other") {
		t.Fatalf("%#v", lines)
	}
	mutableSession(dropdown, "remote").State = core.SessionStateError
	serveAttention(dropdown, []string{"remote"}, core.SessionAttentionFailed)

	lines = appendAttentionState(&output, "open workspace dropdown updates an inactive failure badge", dropdown, 120, 40)
	if !anyLineContains(lines, "×1 Other") {
		t.Fatalf("%#v", lines)
	}
	dropdown.HandleKey(key(tea.KeyDown))
	action = dropdown.HandleKey(key(tea.KeyEnter))
	requireDashboardAction(t, action, DashboardActionSelectWorkspace{WorkspaceID: "other"})
	fmt.Fprintf(&output, "attention dropdown action: %s\n", rustDebug(action))

	priority := dashboardWithAttentionMix()
	priority.SetActiveWorkspace(workspaceID("default"))
	configured := *priority.config.Get()
	configured.Advanced.SessionOrder = core.SessionOrderPriority
	priority.SetConfig(configured)
	appendAttentionState(&output, "priority order lists attention before idle", priority, 140, 40)
	ids := orderedSessionIDs(priority)
	noHeadings := true
	for _, row := range priority.sessionsRows() {
		if _, ok := row.(SessionsRowSession); !ok {
			noHeadings = false
		}
	}
	fmt.Fprintf(&output, "priority session ids: %s; project headings: %v\n", rustDebug(ids), !noHeadings)
	priority.FocusSessions()
	priority.HandleKey(key('1'))
	appendAttentionState(&output, "priority order reveals all rows", priority, 140, 40)
	fmt.Fprintf(&output, "notice: %s; collapsed projects: %s\n",
		optionDebug(priority.notices.Current()), setDebug(*priority.collapsedProjectKeys.Get()))
	mutableSessionDetail(priority, "asks").pendingElicitations = nil
	serveAttention(priority, []string{"asks"}, core.SessionAttentionIdle)
	ids = orderedSessionIDs(priority)
	appendAttentionState(&output, "answering the question lowers it below unread", priority, 140, 40)
	fmt.Fprintf(&output, "priority session ids after answer: %s\n", rustDebug(ids))

	important := dashboardWithAttentionMix()
	for _, id := range []string{"asks", "remote"} {
		mutableSessionDetail(important, id).pendingElicitations = nil
	}
	serveAttention(important, []string{"asks", "remote"}, core.SessionAttentionIdle)
	important.SetActiveWorkspace(workspaceID("default"))
	important.SetCurrentSession(nil)
	serveAttention(important, []string{"done"}, core.SessionAttentionImportant)
	mutableSessionDetail(important, "done").lastAgentMessage = optionalText("Review the deployment result")
	important.config.Mut().Notify.DelaySeconds = 0
	importantCards := core.NewSnapshotMap[string, client.NotificationCard]()
	importantCards.Insert("done", client.NotificationCard{
		Badge: core.NotificationKindImportant,
		Entries: []core.NotificationStackEntry{{
			Kind:             core.NotificationKindImportant,
			CompletedOrdinal: 1,
			Message:          core.EntryMessageNone{},
		}},
	})
	important.notificationCards = &importantCards
	important.SetAlertEvents(0, nil)
	important.SetAlertEvents(1, []client.AlertEvent{{
		Sequence:     1,
		Identity:     `["completion","done",1]`,
		Destination:  "done",
		SessionTitle: "ACP pretty name",
		Text:         "Review the deployment result",
		Kind:         core.NotificationKindImportant,
	}})
	notices := important.NotificationEvents()
	important.notificationCards = nil
	index := slices.IndexFunc(notices, func(notice Notification) bool { return notice.SessionID == "done" })
	if index < 0 {
		t.Fatal("Important is notified despite unread output")
	}
	if notices[index].Level != AttentionLevelImportant || notices[index].Body != "Review the deployment result" {
		t.Fatalf("notice %#v", notices[index])
	}
	if title, ok := important.TerminalTitle(); !ok || title != "mj · 1 important" {
		t.Fatalf("title %q", title)
	}
	fmt.Fprintf(&output, "Important notices: %s; title: %s\n", rustDebug(notices), optionDebug(important.TerminalTitle()))
	appendAttentionState(&output, "Important retains its own glyph, footer and queue level", important, 140, 40)
	if !slices.ContainsFunc(important.AttentionQueue(), func(entry AttentionEntry) bool {
		return entry.SessionID == "done" && entry.Level == AttentionLevelImportant
	}) {
		t.Fatal("done is queued as Important")
	}
	important.SelectActiveSession("asks")
	fmt.Fprintf(&output, "next Important action: %s\n", rustDebug(important.stepAttention(1)))
	if !selectedSessionIs(important, "done") {
		t.Fatal("done is selected")
	}
	core.AssertGolden(t, "testdata", "attention-navigation", output.String())
}

// TestAMinimizedSessionsPaneTitleStillShowsTheFilterChip: RCL-2
// (2026-09-29): with the Sessions pane minimized, the title still says a
// filter is on.
// Hard-won: daa8f51: minimized attention titles dropped the active filter indicator.
func TestAMinimizedSessionsPaneTitleStillShowsTheFilterChip(t *testing.T) {
	dashboard := dashboardWithAttentionMix()
	dashboard.SetActiveWorkspace(workspaceID("default"))
	dashboard.FocusSessions()
	dashboard.HandleKey(key('/'))
	typeKeys(dashboard, "ask")
	dashboard.HandleKey(key(tea.KeyEnter))
	dashboard.SetPaneSize(SupportPaneSessions, core.PaneSizeMinimized)
	if noSessionsFilter(dashboard) {
		t.Fatal("the filter is on")
	}

	lines := drawn(dashboard, 80, 40)
	if dashboard.paneAreas == nil {
		t.Fatal("pane areas")
	}
	pane := dashboard.paneAreas[0]
	row := []rune(lines[pane.Min.Y])
	title := string(row[min(pane.Min.X, len(row)):min(pane.Max.X, len(row))])
	if !strings.Contains(title, "×") {
		t.Fatalf("%q", title)
	}
}

// dashboardWithAnUnreadReplyOffScreen is two running sessions, `alpha` open
// in the conversation pane and selected, and a reply that has landed for
// `beta` while it was off screen. The reply arrives the way the host
// delivers one: a materialized projection whose agent message sits past the
// read frontier the pane left behind.
func dashboardWithAnUnreadReplyOffScreen() *DashboardState {
	alpha := runningSession()
	alpha.ID = "alpha"
	alpha.SessionTitleOverride = optionalText("alpha")
	alpha.CreatedAt = "2026-08-01T00:00:00Z"
	beta := runningSession()
	beta.ID = "beta"
	beta.SessionTitleOverride = optionalText("beta")
	beta.CreatedAt = "2026-08-02T00:00:00Z"
	// The pane read `beta` through its own prompt before it was left for
	// `alpha`; the answer that follows is the unread one.
	beta.ViewedThroughEventOrdinal = 3
	dashboard := dashboardWithSession(alpha)
	dashboard.state.Mut().Sessions.Insert("beta", beta)
	dashboard.SetState(*dashboard.state.Get())
	dashboard.SelectActiveSession("beta")
	dashboard.SetCurrentSession(optionalText("beta"))
	dashboard.SelectActiveSession("alpha")
	dashboard.SetCurrentSession(optionalText("alpha"))
	reply := materializedSessionFor("beta", []*core.TranscriptItem{
		transcriptItem(3, core.TranscriptBodyUser{Content: []json.RawMessage{
			json.RawMessage(`{"type": "text", "text": "beta prompt"}`),
		}}),
		agentMessage(4, "reliability reply: beta prompt"),
	})
	reply.Execution = core.MaterializedExecutionStateIdle{}
	dashboard.ApplyMaterializedSession(&reply)
	// Attention, including unread, arrives in the host publication.
	serveAttention(dashboard, []string{"beta"}, core.SessionAttentionUnread)
	return dashboard
}

// TestTheDoneFilterKeepsTheRowItFoundAndLeavesTheOpenConversationAlone: the
// `d` filter has to keep the row it found, and the row it found is the one
// drawn with the done glyph.
//
// The Sessions selection is what the host opens, and opening a conversation
// marks its answer read. A filter that moved the selection onto the row it
// had just found therefore read that answer, dropped the row out of the
// filter, and left `d` reporting that nothing matches a row still drawn with
// `✓`. A filter is a view: it hides rows without choosing a conversation.
// Hard-won: cf352ae: the done filter read its unread answer and then hid that row.
func TestTheDoneFilterKeepsTheRowItFoundAndLeavesTheOpenConversationAlone(t *testing.T) {
	dashboard := dashboardWithAnUnreadReplyOffScreen()
	lines := drawn(dashboard, 120, 40)
	index := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "beta") })
	if index < 0 {
		t.Fatal("beta has a row")
	}
	if !strings.Contains(lines[index], chat.ThemeGlyphs().Unread) {
		t.Fatalf("the row says the answer is unread: %#v", lines)
	}
	dashboard.FocusSessions()

	dashboard.HandleKey(key('d'))

	requireIDs(t, orderedSessionIDs(dashboard), "alpha", "beta")
	if !selectedSessionIs(dashboard, "alpha") {
		t.Fatal("the filter must not hand the host a different conversation to open")
	}
}

// TestAStateLetterThatHidesEveryRowKeepsAnsweringTheLettersAndEsc: a letter
// that hides every row leaves the pane with no row to select, so the frame
// moves the focus to the Create action. The filter must keep answering from
// there: the empty pane's own line promises that Esc clears the filter, and
// without the letters there is no way back to the sessions at all.
// Hard-won: 6de42d8: an empty filtered list stopped accepting its own clear keys.
func TestAStateLetterThatHidesEveryRowKeepsAnsweringTheLettersAndEsc(t *testing.T) {
	dashboard := dashboardWithAttentionMix()
	dashboard.SetActiveWorkspace(workspaceID("default"))
	dashboard.FocusSessions()

	// Nothing is working, so `w` empties the list.
	dashboard.HandleKey(key('w'))
	lines := drawn(dashboard, 120, 40)
	if len(orderedSessionIDs(dashboard)) != 0 {
		t.Fatal("w empties the list")
	}
	if !anyLineContains(lines, "No sessions match") {
		t.Fatalf("%#v", lines)
	}
	if focus := dashboard.sessionActionFocus; focus == nil || *focus != CommandIdNewSessionWizard {
		t.Fatalf("an empty list leaves the focus on the action row: %v", focus)
	}

	// `a` widens the filter to everything again, and the rows take the focus
	// back from the action row.
	dashboard.HandleKey(key('a'))
	if !noSessionsFilter(dashboard) {
		t.Fatal("a clears the filter")
	}
	requireIDs(t, orderedSessionIDs(dashboard), "asks", "done", "quiet")
	if dashboard.sessionActionFocus != nil {
		t.Fatalf("action focus %v", *dashboard.sessionActionFocus)
	}

	// Another letter still narrows from the empty pane, rather than leaving
	// the person with one filter and no way to change it.
	dashboard.HandleKey(key('w'))
	drawn(dashboard, 120, 40)
	dashboard.HandleKey(key('b'))
	requireIDs(t, orderedSessionIDs(dashboard), "asks")

	// And Esc drops the filter the pane says it drops.
	dashboard.HandleKey(key('w'))
	drawn(dashboard, 120, 40)
	dashboard.HandleKey(key(tea.KeyEscape))
	if !noSessionsFilter(dashboard) {
		t.Fatal("Esc clears the filter")
	}
	requireIDs(t, orderedSessionIDs(dashboard), "asks", "done", "quiet")
}

func suspendPreflight(sessionID string) core.SuspendPreflight {
	return core.SuspendPreflight{SessionID: sessionID}
}

func TestIdleSuspensionRunsImmediatelyAndWorkingSuspensionWarns(t *testing.T) {
	session := runningSession()
	session.ProjectDirectory = optionalText("/srv/project")
	dashboard := dashboardWithSession(session)
	dashboard.FocusSessions()
	requireDashboardAction(t, dashboard.DispatchCommand(CommandIdSuspendSession), DashboardActionSuspendPreflight{SessionID: "session-1"})
	requireDashboardAction(t, dashboard.ApplySuspendPreflight(suspendPreflight("session-1")), DashboardActionSuspend{
		SessionID: "session-1",
		Consent:   core.SuspendConsent{},
	})
	if !isDashboardMode(dashboard) {
		t.Fatalf("mode %#v", dashboard.mode)
	}
	// Working suspension also explains that it interrupts the turn.
	setWorking(dashboard, "session-1")
	requireAttention(t, dashboard, "session-1", AttentionLevelWorking)
	requireDashboardAction(t, dashboard.DispatchCommand(CommandIdSuspendSession), DashboardActionSuspendPreflight{SessionID: "session-1"})
	requireDashboardAction(t, dashboard.ApplySuspendPreflight(suspendPreflight("session-1")), DashboardActionNone{})
	if _, ok := dashboard.mode.(ModeConfirm); !ok {
		t.Fatalf("mode %#v", dashboard.mode)
	}
	lines := drawn(dashboard, 120, 40)
	if !anyLineContains(lines, "The current turn will be interrupted.") {
		t.Fatalf("%#v", lines)
	}
	if !slices.ContainsFunc(lines, func(line string) bool {
		return strings.Contains(line, "c Cancel") && strings.Contains(line, "a Continue anyway")
	}) {
		t.Fatalf("%#v", lines)
	}
	requireDashboardAction(t, dashboard.HandleKey(key('a')), DashboardActionSuspend{
		SessionID: "session-1",
		Consent:   core.SuspendConsent{},
	})
	if !isDashboardMode(dashboard) {
		t.Fatalf("mode %#v", dashboard.mode)
	}

	requireDashboardAction(t, dashboard.DispatchCommand(CommandIdRestartSession), DashboardActionNone{})
	requireDashboardAction(t, dashboard.HandleKey(key('c')), DashboardActionNone{})
	if !isDashboardMode(dashboard) {
		t.Fatalf("mode %#v", dashboard.mode)
	}
}

func TestIdleManagedCloneSuspensionPreflightsUntrackedInventory(t *testing.T) {
	session := runningSession()
	root := "/srv/project/.mj/clones/session-1"
	session.ProjectDirectory = &root
	session.ManagedWorktree = &core.ManagedWorktree{
		Kind:                   core.ManagedCheckoutKindClone,
		SourceProjectDirectory: "/srv/project",
		SourceRepository:       "/srv/project",
		WorktreeRoot:           root,
		Branch:                 "master",
		Target:                 core.ManagedWorktreeTargetLocal{},
		BaseCommit:             optionalText(strings.Repeat("1", 40)),
	}
	dashboard := dashboardWithSession(session)
	dashboard.FocusSessions()
	requireDashboardAction(t, dashboard.DispatchCommand(CommandIdSuspendSession), DashboardActionSuspendPreflight{SessionID: "session-1"})
	preflight := suspendPreflight("session-1")
	preflight.Untracked = append(preflight.Untracked, core.UntrackedPath{
		Path:     "notes.txt",
		Bytes:    0,
		Identity: core.NewFileIdentity(""),
	})
	expectedConsent := preflight.ShownConsent()
	requireDashboardAction(t, dashboard.ApplySuspendPreflight(preflight), DashboardActionNone{})
	lines := drawn(dashboard, 120, 40)
	if !anyLineContains(lines, "notes.txt") {
		t.Fatalf("%#v", lines)
	}
	requireDashboardAction(t, dashboard.HandleKey(key('d')), DashboardActionSuspend{
		SessionID: "session-1",
		Consent:   expectedConsent,
	})
}

func gitStatusFixture() core.SessionGitStatus {
	aheadBehind := "2\t1"
	return core.ParseGitStatus("/work", "feature/x", &aheadBehind, "12\t3\tsrc/main.rs\x00", " M src/main.rs\x00?? notes.md\x00")
}

// TestALongBranchNameIsElidedInTheMiddleSoTheMarkerKeepsItsCounts: every
// session created with a managed worktree gets a `mj/<32 hex>` branch, which
// is wider than the sidebar, so dropping the whole marker hid the feature at
// the widths people use. The middle of the name is what nobody reads: elide
// it and the marker keeps its ahead, behind, and changed counts.
// Hard-won: ddb00ad: long generated branch names hid all checkout status at normal widths.
func TestALongBranchNameIsElidedInTheMiddleSoTheMarkerKeepsItsCounts(t *testing.T) {
	session := runningSession()
	session.SessionTitleOverride = optionalText("alpha")
	session.Target = core.TargetLocatorLocalBare{WorkerRoot: "/work"}
	dashboard := dashboardWithSession(session)
	dashboard.FocusSessions()
	// The 35 characters a managed worktree's own branch name costs.
	branch := "mj/d45dc90af02510176be667cf8b330580"
	if count := len([]rune(branch)); count != 35 {
		t.Fatalf("%d characters", count)
	}
	aheadBehind := "2\t1"
	dashboard.SetGitStatus("session-1", client.DaemonOk(
		core.ParseGitStatus("/work", branch, &aheadBehind, "12\t3\tsrc/main.rs\x00", " M src/main.rs\x00?? notes.md\x00"),
	))
	// A third of a 100-column terminal, floored at 40, is a 40-column pane.
	lines := drawn(dashboard, 100, 40)
	index := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "alpha") })
	if index < 0 {
		t.Fatalf("the session row: %#v", lines)
	}
	row := lines[index]
	// The prefix and the last characters of the name survive; the middle
	// does not, which is what makes room for the counts.
	for _, want := range []string{"⎇ mj/d45", "…", "80 ↑1 ↓2 ±2"} {
		if !strings.Contains(row, want) {
			t.Fatalf("%q missing from %q", want, row)
		}
	}
}

func TestGitProbesCoverVisibleLiveSessionsAboutOnceAMinute(t *testing.T) {
	session := runningSession()
	session.Target = core.TargetLocatorLocalBare{WorkerRoot: "/work"}
	dashboard := dashboardWithSession(session)
	start := time.Now()
	requireIDs(t, dashboard.GitProbeCandidates(start, 2), "session-1")
	if candidates := dashboard.GitProbeCandidates(start, 2); len(candidates) != 0 {
		t.Fatalf("%q", candidates)
	}
	if candidates := dashboard.GitProbeCandidates(start.Add(30*time.Second), 2); len(candidates) != 0 {
		t.Fatalf("%q", candidates)
	}
	requireIDs(t, dashboard.GitProbeCandidates(start.Add(61*time.Second), 2), "session-1")
	// A stopped session's target is gone, so there is nothing to read.
	mutableSession(dashboard, "session-1").State = core.SessionStateStopped
	if candidates := dashboard.GitProbeCandidates(start.Add(200*time.Second), 2); len(candidates) != 0 {
		t.Fatalf("%q", candidates)
	}
}

// TestGitProbesReachEverySessionWhenMoreAreDueThanOnePassReads: launch
// finding D-3: after a restart every session is due at once. The host reads
// only a few per pass, so a session it did not read must stay due; marking
// all of them as read left all but the first few without a branch marker for
// good.
// Hard-won: 3ea7e85: the per-pass limit permanently starved sessions beyond the first batch.
func TestGitProbesReachEverySessionWhenMoreAreDueThanOnePassReads(t *testing.T) {
	target := core.TargetLocatorLocalBare{WorkerRoot: "/work"}
	first := runningSession()
	first.Target = target
	dashboard := dashboardWithSession(first)
	for _, id := range []string{"session-2", "session-3", "session-4"} {
		session := runningSession()
		session.ID = id
		session.Target = target
		dashboard.state.Mut().Sessions.Insert(id, session)
	}
	publishFixtureVerdicts(dashboard)
	start := time.Now()

	probed := dashboard.GitProbeCandidates(start, 2)
	if len(probed) != 2 {
		t.Fatalf("one pass reads at most the limit: %q", probed)
	}
	probed = append(probed, dashboard.GitProbeCandidates(start.Add(time.Second), 2)...)
	slices.Sort(probed)

	requireIDs(t, probed, "session-1", "session-2", "session-3", "session-4")
}

func TestTheASCIISymbolSetDrawsTheDashboardWithoutNonASCIIGlyphs(t *testing.T) {
	dashboard := dashboardWithAttentionMix()
	dashboard.SetActiveWorkspace(workspaceID("default"))
	dashboard.SetGitStatus("asks", client.DaemonOk(gitStatusFixture()))
	configured := *dashboard.config.Get()
	ascii := core.SymbolSetAscii
	configured.Advanced.Symbols = &ascii
	dashboard.SetConfig(configured)
	lines := drawn(dashboard, 120, 40)
	offenders := map[string]struct{}{}
	for _, line := range lines {
		for _, character := range line {
			if character > 0x7f {
				offenders[string(character)] = struct{}{}
			}
		}
	}
	if len(offenders) != 0 {
		t.Fatalf("non-ASCII glyphs drawn: %s\n%#v", setDebug(offenders), lines)
	}
	if !anyLineContains(lines, "+---") {
		t.Fatalf("ASCII borders: %#v", lines)
	}
	if !anyLineContains(lines, "ACP pretty name  br feature/x") {
		t.Fatalf("%#v", lines)
	}
	wide := drawn(dashboard, 160, 40)
	if !anyLineContains(wide, "br feature/x +1 -2 ~2") {
		t.Fatalf("%#v", wide)
	}
	// The chord hints are joined by the ASCII separator.
	if last := wide[len(wide)-1]; !strings.Contains(last, "c create - g sessions") {
		t.Fatalf("ASCII footer separators: %s", last)
	}

	// The default set is unchanged.
	configured = *dashboard.config.Get()
	unicodeSymbols := core.SymbolSetUnicode
	configured.Advanced.Symbols = &unicodeSymbols
	dashboard.SetConfig(configured)
	lines = drawn(dashboard, 120, 40)
	if !anyLineContains(lines, "╭") {
		t.Fatalf("%#v", lines)
	}
}

// Hard-won: b9e5e3b: failed targets were still attached until the open attempt could not finish.
func TestAFailedSessionShowsItsErrorInsteadOfAnAttachThatCannotFinish(t *testing.T) {
	session := runningSession()
	session.State = core.SessionStateError
	session.LastError = optionalText("connect worker at /var/lib/hel/workers/abc: no such file")
	dashboard := dashboardWithSession(session)
	dashboard.FocusSessions()
	if !dashboard.SessionFailed("session-1") {
		t.Fatal("the session failed")
	}
	lines := drawn(dashboard, 120, 40)
	text := strings.Join(lines, "\n")
	for _, want := range []string{"Session failed", "connect worker at", "Enter in Sessions opens its transcript or recovers it"} {
		if !strings.Contains(text, want) {
			t.Fatalf("%q missing: %#v", want, lines)
		}
	}
	if strings.Contains(text, "Bringing your conversation") {
		t.Fatalf("%#v", lines)
	}
	// Enter still asks what to do rather than attaching blindly.
	dashboard.HandleKey(key(tea.KeyEnter))
	if _, ok := dashboard.mode.(ModeConfirm); !ok {
		t.Fatalf("%#v", dashboard.mode)
	}
}

// Hard-won: b9e5e3b: long failure notices lost their diagnostic tail at terminal width.
func TestTheNoticeLogWrapsALongFailureInsteadOfCuttingItsTail(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	long := fmt.Sprintf(
		"Credential sync for profile codex (session 2ac006c1) failed: relay proxy stderr (last 4 lines): %s the-very-last-word",
		strings.Repeat("x", 150),
	)
	dashboard.SetFailureNotice(long)
	dashboard.DispatchCommand(CommandIdNoticeLog)
	lines := drawn(dashboard, 120, 40)
	text := strings.Join(lines, "\n")
	if !strings.Contains(text, "the-very-last-word") {
		t.Fatalf("%#v", lines)
	}
	first := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "Credential sync for profile") })
	if first < 0 {
		t.Fatal("the message starts")
	}
	if !strings.Contains(lines[first], "ago") {
		t.Fatalf("%#v", lines)
	}
	if !strings.Contains(lines[first+1], "xxxx") && !strings.Contains(lines[first+2], "xxxx") {
		t.Fatalf("continuation lines under the age column: %#v", lines)
	}
}

// TestAFinishedNativeChildShowsItsStoredTranscriptAndOpensOnEnter: R10-2: a
// native Codex child that had finished while its parent was still running
// read "No messages yet" in the Sub-agents view, its conversation never
// appeared, and Enter did nothing, although the store held its transcript.
// The host empties any pane whose session `pane_session_is_suspended` calls
// suspended (R4-11), and a finished native child's presentation row is
// `Stopped`, so Browse let it go as soon as the selection put it there.
// Hard-won: 936d6ee: finished native children appeared empty and could not be opened.
func TestAFinishedNativeChildShowsItsStoredTranscriptAndOpensOnEnter(t *testing.T) {
	dashboard, parentID, id := dashboardWithFinishedNativeChild()
	dashboard.OpenSubagentWorkspace(parentID)
	dashboard.FocusSessions()
	dashboard.HandleKey(key('a'))
	dashboard.SelectActiveSession(id)
	if !selectedSessionIs(dashboard, id) {
		t.Fatal("the child is selected")
	}
	if dashboard.PaneSessionIsSuspended(id) {
		t.Fatal("a finished native child is read from the store; no pane lets it go")
	}

	// Browse follows the selection onto the child.
	browse := dashboard.BrowsePane()
	dashboard.SetPaneSession(browse, &id)
	lines := drawn(dashboard, 140, 40)
	screen := strings.Join(lines, "\n")
	if !strings.Contains(screen, "Reading calc.py") || !strings.Contains(screen, "Native agent") {
		t.Fatalf("the child's stored transcript is drawn in its pane: %s", screen)
	}
	// The pane's title row names the child too; the row is in Sessions.
	row := slices.IndexFunc(lines, func(line string) bool {
		return strings.Contains(line, "Review calc · completed") && !strings.Contains(line, "Conversation")
	})
	if row < 0 {
		t.Fatal("the child's row")
	}
	rowText := strings.Join(lines[row:row+4], "\n")
	if strings.Contains(rowText, "No messages yet") {
		t.Fatalf("the row shows the child's last message: %s", rowText)
	}
	if !strings.Contains(rowText, "calc.py adds and subtracts") {
		t.Fatal(rowText)
	}

	// Enter on the row opens the child's conversation.
	dashboard.FocusSessions()
	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionOpen{SessionID: id})
}

// TestANativeChildPaneHeaderNamesTheChildFirst: launch finding R12-3 (also
// R11 tmux/012): a native child's pane header read "Browse | Conversation d ·
// availability unknown". The pane chrome draws its label over the left of the
// title row and its chips over the right, and the native pane's own title
// started under the label, which covered " Review calc · complete". The title
// starts after the label, names the child first, and drops status words from
// the end when the row is short. At 80 columns the pane is 42 wide and the
// chrome leaves 7 columns, so the name itself is cut.
// Hard-won: 90ef5fd: pane chrome obscured most of a native child’s title.
func TestANativeChildPaneHeaderNamesTheChildFirst(t *testing.T) {
	dashboard, parentID, id := dashboardWithFinishedNativeChild()
	dashboard.OpenSubagentWorkspace(parentID)
	dashboard.FocusSessions()
	dashboard.HandleKey(key('a'))
	browse := dashboard.BrowsePane()
	dashboard.SetPaneSession(browse, &id)
	for _, expected := range []struct {
		width int
		title string
	}{
		{140, "Review calc · completed · availability unknown "},
		{100, "Review calc · completed "},
		{90, "Review calc "},
		{80, "Rev…"},
	} {
		lines := drawn(dashboard, expected.width, 40)
		index := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "Browse | Conversation") })
		if index < 0 {
			t.Fatal("the pane's title row")
		}
		header := lines[index]
		if !strings.Contains(header, fmt.Sprintf("Browse | Conversation  %s─", expected.title)) {
			t.Fatalf("%d columns: %s", expected.width, header)
		}
	}
}

func nativeAgentView(agent core.NativeAgent) core.NativeAgentView {
	return core.NativeAgentView{
		GenerationOrdinal: 1,
		Projection:        core.MaterializedSessionEmpty(agent.ViewID()),
		Agent:             agent,
	}
}

func TestNativeAgentPaneSurvivesRefreshAndBlocksManagedSessionActions(t *testing.T) {
	parent := runningSession()
	dashboard := dashboardWithSession(parent)
	agent := core.NativeAgent{
		OwnerSessionID: parent.ID,
		SessionID:      "native-child",
		Name:           "Inspect parser",
		Task:           "Find parser errors",
		Capabilities:   core.NativeAgentCapabilities{Cancel: true, Close: false},
		State:          core.NativeAgentStateRunning,
	}
	id := agent.ViewID()
	serveAttention(dashboard, []string{id}, core.SessionAttentionWorking)

	dashboard.SetNativeAgents([]core.NativeAgentView{nativeAgentView(agent)})
	if count := dashboard.SubagentCountFor(parent.ID); count != 1 {
		t.Fatalf("sub-agent count %d", count)
	}
	if slices.Contains(orderedSessionIDs(dashboard), id) {
		t.Fatal("the native child is not a top-level row")
	}
	dashboard.OpenSubagentWorkspace(parent.ID)
	if !selectedSessionIs(dashboard, id) {
		t.Fatal("the native child is selected")
	}
	if len(dashboard.orderedSessions()) != 1 {
		t.Fatal("one child row")
	}
	state := core.NewStateState()
	state.Sessions.Insert(parent.ID, parent)
	dashboard.SetState(state)
	if !selectedSessionIs(dashboard, id) {
		t.Fatal("the refresh keeps the native child selected")
	}
	requireDashboardAction(t, dashboard.DispatchCommand(CommandIdSuspendSession), DashboardActionNone{})
	requireDashboardAction(t, chord(dashboard, CommandIdSuspendSession), DashboardActionNone{})
	if !isDashboardMode(dashboard) {
		t.Fatal("native agents close through their parent")
	}
	if stop, ok := dashboard.HandleKey(key('s')).(DashboardActionStopNativeAgent); !ok || stop.Owner != parent.ID || stop.Child != "native-child" {
		t.Fatal("s stops the native agent")
	}
	requireDashboardAction(t, dashboard.HandleKey(key('s')), DashboardActionNone{})
	dashboard.NativeAgentStopFinished(parent.ID, "native-child", errors.New("offline"))
	if _, ok := dashboard.HandleKey(key('s')).(DashboardActionStopNativeAgent); !ok {
		t.Fatal("a failed stop can be retried")
	}
	// Returning to a managed owner retains that owner's own parent
	// workspace.
	ancestor := parent
	ancestor.ID = "ancestor"
	dashboard.state.Mut().Sessions.Insert(ancestor.ID, ancestor)
	dashboard.state.Mut().Subagents.Insert(parent.ID, core.SubagentMetadata{
		Kind:            core.SubagentKindWorker,
		ChildSessionID:  parent.ID,
		ParentSessionID: ancestor.ID,
		TaskName:        "managed owner",
		ProfileID:       parent.LastProfile,
		RequestKey:      "request",
		CreatedAt:       parent.CreatedAt,
	})
	if open, ok := dashboard.HandleKey(key('p')).(DashboardActionOpen); !ok || open.SessionID != parent.ID {
		t.Fatal("p opens the managed owner")
	}
	if id, ok := dashboard.SubagentParentID(); !ok || id != "ancestor" {
		t.Fatalf("parent %q", id)
	}
	if !selectedSessionIs(dashboard, parent.ID) {
		t.Fatal("the owner is selected")
	}
}

// Hard-won: 6eb3146b8: active child filters hid native navigation and parent return.
func TestNativeNestedScopeKeysPreserveFiltersAndFilterEditing(t *testing.T) {
	parent := runningSession()
	dashboard := dashboardWithSession(parent)
	child := core.NativeAgent{
		OwnerSessionID: parent.ID,
		SessionID:      "child",
		Name:           "Child",
		State:          core.NativeAgentStateRunning,
	}
	grandchild := child
	grandchild.SessionID = "grandchild"
	grandchild.ParentSessionID = optionalText("child")
	grandchild.Name = "Grandchild"
	childID := child.ViewID()
	grandchildID := grandchild.ViewID()
	dashboard.SetNativeAgents([]core.NativeAgentView{nativeAgentView(child), nativeAgentView(grandchild)})

	serveAttention(dashboard, []string{childID, grandchildID}, core.SessionAttentionWorking)
	dashboard.FocusSessions()
	dashboard.HandleKey(key('I'))
	dashboard.HandleKey(key('/'))
	typeKeys(dashboard, "session")
	dashboard.HandleKey(key(tea.KeyEnter))
	topFilter := dashboard.sessionsFilterLabel()

	parentIs := func(want string) bool {
		id, ok := dashboard.SubagentParentID()
		if want == "" {
			return !ok
		}
		return ok && id == want
	}
	opens := func(want string) bool {
		open, ok := dashboard.HandleKey(key('p')).(DashboardActionOpen)
		return ok && open.SessionID == want
	}

	dashboard.OpenSubagentWorkspace(parent.ID)
	if !selectedSessionIs(dashboard, childID) {
		t.Fatal("the child is selected")
	}
	requireSessionsFilterState(t, dashboard, SessionStateFilterNotIdle)
	dashboard.HandleKey(key(tea.KeyRight))
	if !parentIs(childID) {
		t.Fatal("Right enters the child's scope")
	}
	if !selectedSessionIs(dashboard, grandchildID) {
		t.Fatal("the grandchild is selected")
	}
	if load, ok := dashboard.HandleKey(key(tea.KeyPgUp)).(DashboardActionLoadNativeAgentHistory); !ok || load.Owner != parent.ID || load.Child != "grandchild" {
		t.Fatal("PageUp loads the grandchild's history")
	}

	dashboard.HandleKey(key('/'))
	typeKeys(dashboard, "grandchild")
	dashboard.HandleKey(key(tea.KeyEnter))
	if !parentIs(childID) {
		t.Fatal("editing Enter belongs to the filter")
	}
	if !strings.Contains(dashboard.sessionsFilterLabel(), "/grandchild") {
		t.Fatal(dashboard.sessionsFilterLabel())
	}
	dashboard.HandleKey(key('a'))
	if !strings.Contains(dashboard.sessionsFilterLabel(), "/grandchild") {
		t.Fatal(dashboard.sessionsFilterLabel())
	}
	if state := sessionsFilterState(dashboard); state != nil {
		t.Fatalf("filter state %v", *state)
	}
	if !opens(childID) {
		t.Fatal("p opens the child")
	}
	if !parentIs(parent.ID) {
		t.Fatal("p returns to the parent's scope")
	}
	requireSessionsFilterState(t, dashboard, SessionStateFilterNotIdle)
	dashboard.HandleKey(key(tea.KeyEnter))
	if !parentIs(childID) {
		t.Fatal("Enter enters the child's scope")
	}
	if !opens(childID) {
		t.Fatal("p opens the child")
	}
	if !opens(parent.ID) {
		t.Fatal("p opens the parent")
	}
	if !parentIs("") {
		t.Fatal("the top level is shown")
	}
	if label := dashboard.sessionsFilterLabel(); label != topFilter {
		t.Fatalf("label %q", label)
	}
	dashboard.OpenSubagentWorkspace(parent.ID)
	dashboard.OpenSubagentWorkspace(childID)
	if !strings.Contains(dashboard.sessionsFilterLabel(), "/grandchild") {
		t.Fatal(dashboard.sessionsFilterLabel())
	}
	if state := sessionsFilterState(dashboard); state != nil {
		t.Fatalf("filter state %v", *state)
	}
}

func TestResizeModeRoutesRepeatedMotionsConsumesTextAndExits(t *testing.T) {
	dashboard := dashboardWithTwoSessions()
	requireSplit(t, dashboard, optionalText("session-2"))
	dashboard.focus = FocusPrompt
	chord(dashboard, CommandIdResizeMode)
	if !dashboard.ResizeModeActive() {
		t.Fatal("resize mode is on")
	}
	before := reflectSnapshot(dashboard.ConversationLayoutFor("default"))
	repeat := key('h')
	repeat.IsRepeat = true
	route(dashboard, []tea.KeyMsg{repeat, repeat})
	if !dashboard.ResizeModeActive() {
		t.Fatal("repeats stay in resize mode")
	}
	if reflectSnapshot(dashboard.ConversationLayoutFor("default")) == before {
		t.Fatal("the repeats resized the panes")
	}
	if route := dashboard.RouteBoundKey(key('q')); !reflect.DeepEqual(route, KeyRouteConsumed{}) {
		t.Fatalf("q is consumed: %#v", route)
	}
	if route := dashboard.RouteBoundKeyEvent(tea.PasteMsg{Content: "do not submit"}); !reflect.DeepEqual(route, KeyRouteConsumed{}) {
		t.Fatalf("a paste is consumed: %#v", route)
	}
	lines := drawn(dashboard, 120, 40)
	if !strings.Contains(lines[len(lines)-1], "Resize panes:") {
		t.Fatal(lines[len(lines)-1])
	}
	route(dashboard, []tea.KeyMsg{key(tea.KeyEscape)})
	if dashboard.ResizeModeActive() {
		t.Fatal("Esc leaves resize mode")
	}
	if route := dashboard.RouteBoundKey(key('h')); !reflect.DeepEqual(route, KeyRouteForward{}) {
		t.Fatalf("h is forwarded: %#v", route)
	}
	chord(dashboard, CommandIdResizeMode)
	dashboard.DispatchCommand(CommandIdPalette)
	if dashboard.ResizeModeActive() {
		t.Fatal("the palette leaves resize mode")
	}
	dashboard.CancelModal()
	if dashboard.ResizeModeActive() {
		t.Fatal("cancelling stays out of resize mode")
	}
}

func TestResizeModeRespectsCustomCommandsAndWorkspaceChanges(t *testing.T) {
	dashboard := dashboardWithTwoSessions()
	configured := config()
	configured.Keys.ResizeMode = core.BindingConfigFrom("prefix+e")
	configured.Keys.Refresh = core.BindingConfigFrom("f5")
	dashboard.SetConfig(configured)
	requireDashboardAction(t, chord(dashboard, CommandIdResizeMode), DashboardActionNone{})
	if dashboard.ResizeModeActive() {
		t.Fatal("a single pane cannot resize")
	}
	requireSplit(t, dashboard, optionalText("session-2"))
	chord(dashboard, CommandIdZoomPane)
	if !dashboard.ConversationZoomed() {
		t.Fatal("the pane is zoomed")
	}
	chord(dashboard, CommandIdResizeMode)
	if dashboard.ConversationZoomed() {
		t.Fatal("resize mode unzooms")
	}
	requireDashboardAction(t, route(dashboard, []tea.KeyMsg{key(tea.KeyF5)}), DashboardActionRefreshAll{})
	if dashboard.ResizeModeActive() {
		t.Fatal("a command leaves resize mode")
	}
	chord(dashboard, CommandIdResizeMode)
	dashboard.SetActiveWorkspace(nil)
	if dashboard.ResizeModeActive() {
		t.Fatal("a workspace change leaves resize mode")
	}
}

// TestASubagentQuestionMarksItsParentForAttention: launch finding R11-1: a
// Claude sub-agent sat on a permission question in the Sub-agents view while
// its parent's row read only "Working" with no attention mark, so nobody knew
// to look. A child's question marks its parent the way the parent's own
// question would: the row's symbol, the attention queue, and the row says
// whose question it is.
// Hard-won: 91244ea: a child permission question left its parent without an attention mark.
// Hard-won: #1451: the restored mark hid the parent's running turn from turn controls.
func TestASubagentQuestionMarksItsParentForAttention(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	childForm := func() core.ElicitationRequest {
		request := question("child-session")
		request.ToolPermission = &core.ToolPermissionForm{Skip: true}
		return request
	}
	mutableSession(dashboard, "child-session").ACPSessionTitle = optionalText("Answer project codename")
	setWorking(dashboard, parent)
	setWorking(dashboard, "child-session")
	dashboard.SetCurrentSession(nil)
	requireAttention(t, dashboard, parent, AttentionLevelWorking)

	mutableSessionDetail(dashboard, "child-session").pendingElicitations = []core.ElicitationRequest{childForm()}
	serveAttention(dashboard, []string{parent, "child-session"}, core.SessionAttentionNeedsInput)

	dashboard.SetFormHolders(holders([2]string{parent, "child-session"}, [2]string{"child-session", "child-session"}))

	// The parent's own turn still runs, so its turn controls still apply.
	dashboard.commandSessionOverride = &parent
	requireDashboardAction(t, dashboard.DispatchCommand(CommandIdInterruptTurn), DashboardActionInterruptTurn{SessionID: parent})
	dashboard.commandSessionOverride = nil

	requireAttention(t, dashboard, parent, AttentionLevelWaiting)
	var queue []string
	for _, entry := range dashboard.AttentionQueue() {
		queue = append(queue, entry.SessionID)
	}
	if !slices.Equal(queue, []string{parent}) {
		t.Fatalf("the muted child publishes through its unmuted parent: %q", queue)
	}
	lines := drawn(dashboard, 120, 40)
	row := slices.IndexFunc(lines, func(line string) bool { return strings.Contains(line, "ACP pretty name") })
	if row < 0 {
		t.Fatal("the parent's row is drawn")
	}
	if !strings.Contains(lines[row], chat.ThemeGlyphs().Waiting+" ACP pretty name") {
		t.Fatal(lines[row])
	}
	if !strings.Contains(lines[row+1], "Sub-agent question") {
		t.Fatal(lines[row+1])
	}
	// Answering the child's question clears the parent's mark.
	mutableSessionDetail(dashboard, "child-session").pendingElicitations = nil
	serveAttention(dashboard, []string{parent, "child-session"}, core.SessionAttentionWorking)

	requireAttention(t, dashboard, parent, AttentionLevelWorking)

	dashboard.SetFormHolders(core.NewSnapshotMap[string, string]())
}

// TestAReviewerQuestionNamesTheReviewerAndLeadsToItsWorkspace: reviewers
// are listed in their own workspace, not among the Sub-agents, so a
// reviewer's open form names the reviewer and leads there.
// Hard-won: #1451: a reviewer's open form read as a sub-agent question and led to the Sub-agents view, which does not list reviewers.
func TestAReviewerQuestionNamesTheReviewerAndLeadsToItsWorkspace(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	state := *dashboard.state.Get()
	state.Subagents.GetMut("child-session").Kind = core.SubagentKindReviewer
	dashboard.SetState(state)
	setWorking(dashboard, parent)
	setWorking(dashboard, "child-session")
	dashboard.SetCurrentSession(nil)
	mutableSessionDetail(dashboard, "child-session").pendingElicitations = []core.ElicitationRequest{question("child-session")}
	serveAttention(dashboard, []string{parent, "child-session"}, core.SessionAttentionNeedsInput)

	dashboard.SetFormHolders(holders([2]string{parent, "child-session"}, [2]string{"child-session", "child-session"}))
	lines := drawn(dashboard, 120, 40)
	if !anyLineContains(lines, "Reviewer question") {
		t.Fatalf("%#v", lines)
	}
	dashboard.stepAttention(1)
	if !dashboard.ReviewersWorkspaceOpen() {
		t.Fatal("the reviewer workspace is open")
	}
	if !selectedSessionIs(dashboard, "child-session") {
		t.Fatal("the reviewer is selected")
	}
}

// TestAChildsReviewerQuestionReachesTheTopLevelRow: a child may run its own
// review. Its reviewer's open form marks the top-level row, names the
// reviewer, and leads to it.
// Hard-won: #1451: a child's reviewer holding an approval marked only the hidden child, so nothing led the person to it.
func TestAChildsReviewerQuestionReachesTheTopLevelRow(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	state := *dashboard.state.Get()
	reviewer, _ := state.Sessions.Get("child-session")
	reviewer.ID = "reviewer-session"
	relation, _ := state.Subagents.Get("child-session")
	relation.Kind = core.SubagentKindReviewer
	relation.ChildSessionID = reviewer.ID
	relation.ParentSessionID = "child-session"
	relation.RequestKey = "review-1"
	state.Sessions.Insert(reviewer.ID, reviewer)
	state.Subagents.Insert("reviewer-session", relation)
	dashboard.SetState(state)
	for _, id := range []string{parent, "child-session", "reviewer-session"} {
		setWorking(dashboard, id)
	}
	dashboard.SetCurrentSession(nil)
	mutableSessionDetail(dashboard, "reviewer-session").pendingElicitations = []core.ElicitationRequest{question("reviewer-session")}
	serveAttention(dashboard, []string{parent, "child-session", "reviewer-session"}, core.SessionAttentionNeedsInput)

	dashboard.SetFormHolders(holders(
		[2]string{parent, "reviewer-session"},
		[2]string{"child-session", "reviewer-session"},
		[2]string{"reviewer-session", "reviewer-session"},
	))
	requireAttention(t, dashboard, parent, AttentionLevelWaiting)
	lines := drawn(dashboard, 120, 40)
	if !anyLineContains(lines, "Reviewer question") {
		t.Fatalf("%#v", lines)
	}
	dashboard.stepAttention(1)
	if !dashboard.ReviewersWorkspaceOpen() {
		t.Fatal("the reviewer workspace is open")
	}
	if !selectedSessionIs(dashboard, "reviewer-session") {
		t.Fatal("the reviewer is selected")
	}
}

// TestParentSuspensionWarnsOnlyAboutSubagentsStillAtTheirTask: a suspend
// stops the parent's sub-agents without a checkpoint of their own. It asks
// first only when one of them is still at its task, and says, by its listed
// title, that suspending stops it (R15-4); an idle child has handed back and
// stops silently.
func TestParentSuspensionWarnsOnlyAboutSubagentsStillAtTheirTask(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	// A raw project has no clone whose publication needs confirming.
	mutableSession(dashboard, parent).ProjectDirectory = optionalText("/srv/project")
	mutableSession(dashboard, "child-session").SessionTitleOverride = optionalText("sleep-100")
	dashboard.FocusSessions()
	requireAttention(t, dashboard, parent, AttentionLevelIdle)
	requireDashboardAction(t, dashboard.DispatchCommand(CommandIdSuspendSession), DashboardActionSuspendPreflight{SessionID: parent})
	requireDashboardAction(t, dashboard.ApplySuspendPreflight(suspendPreflight(parent)), DashboardActionSuspend{
		SessionID: parent,
		Consent:   core.SuspendConsent{},
	})

	setWorking(dashboard, "child-session")

	requireAttention(t, dashboard, parent, AttentionLevelIdle)
	requireDashboardAction(t, dashboard.DispatchCommand(CommandIdSuspendSession), DashboardActionSuspendPreflight{SessionID: parent})
	requireDashboardAction(t, dashboard.ApplySuspendPreflight(suspendPreflight(parent)), DashboardActionNone{})
	dialog, ok := dashboard.mode.(ModeConfirm)
	if !ok {
		t.Fatalf("mode %#v", dashboard.mode)
	}
	if suspend, ok := dialog.confirmation.(ConfirmationSuspendSession); !ok || suspend.Interrupting {
		t.Fatalf("confirmation %#v", dialog.confirmation)
	}
	text := strings.Join(drawn(dashboard, 120, 40), "\n")
	if !strings.Contains(text, "Sub-agent \"sleep-100\" has not handed back; suspending stops it.") {
		t.Fatal(text)
	}
	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEscape)), DashboardActionNone{})
	if !isDashboardMode(dashboard) {
		t.Fatalf("mode %#v", dashboard.mode)
	}
}

// TestSuspendConfirmationAndRenameDialogNameTheSessionByItsTitle: launch
// finding B-3: the row menu is titled with the session's name, so the
// Suspend confirmation and the Rename dialog must name it the same way
// instead of by its id.
// Hard-won: 78e2c60: session dialogs showed opaque ids instead of the known title.
func TestSuspendConfirmationAndRenameDialogNameTheSessionByItsTitle(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	started := uint64(1)
	mutableSessionDetail(dashboard, "session-1").currentTurnStartedAt = &started
	chord(dashboard, CommandIdSuspendSession)
	suspend := strings.Join(drawn(dashboard, 120, 40), "\n")
	if !strings.Contains(suspend, "Session: ACP pretty name") || strings.Contains(suspend, "Session: session-1") {
		t.Fatal(suspend)
	}
	dashboard.HandleKey(key(tea.KeyEscape))

	chord(dashboard, CommandIdRenameSession)
	rename := strings.Join(drawn(dashboard, 120, 40), "\n")
	if !strings.Contains(rename, "Session: ACP pretty name") || strings.Contains(rename, "Session: session-1") {
		t.Fatal(rename)
	}
}

// TestDialogsNameAnUnnamedSessionByTheTitleItWasCreatedWith: R4-12: a
// session nobody has named yet keeps the title it was created with ("project
// via claude"), and the session list shows that title (R2-8). The palette
// heading, the Suspend confirmation and the Rename dialog named it by its id
// instead.
// Hard-won: f639b25: dialogs discarded the created title and showed an opaque id.
func TestDialogsNameAnUnnamedSessionByTheTitleItWasCreatedWith(t *testing.T) {
	session := runningSession()
	session.ACPSessionTitle = nil
	session.Title = "project via claude"
	dashboard := dashboardWithSession(session)
	started := uint64(1)
	mutableSessionDetail(dashboard, "session-1").currentTurnStartedAt = &started

	dashboard.beginSessionPalette()
	palette := strings.Join(drawn(dashboard, 120, 40), "\n")
	if !strings.Contains(palette, "project via claude") {
		t.Fatal(palette)
	}
	dashboard.HandleKey(key(tea.KeyEscape))

	chord(dashboard, CommandIdSuspendSession)
	suspend := strings.Join(drawn(dashboard, 120, 40), "\n")
	if !strings.Contains(suspend, "Session: project via claude") {
		t.Fatal(suspend)
	}
	dashboard.HandleKey(key(tea.KeyEscape))

	chord(dashboard, CommandIdRenameSession)
	rename := strings.Join(drawn(dashboard, 120, 40), "\n")
	if !strings.Contains(rename, "Session: project via claude") || strings.Contains(rename, "Session: session-1") {
		t.Fatal(rename)
	}
}

// TestSuspendConfirmationFallsBackToTheIDForAnUntitledSession: a session
// with no name at all falls back to its id, the only name it has.
func TestSuspendConfirmationFallsBackToTheIDForAnUntitledSession(t *testing.T) {
	session := runningSession()
	session.ACPSessionTitle = nil
	session.Title = ""
	dashboard := dashboardWithSession(session)
	started := uint64(1)
	mutableSessionDetail(dashboard, "session-1").currentTurnStartedAt = &started
	chord(dashboard, CommandIdSuspendSession)
	suspend := strings.Join(drawn(dashboard, 120, 40), "\n")
	if !strings.Contains(suspend, "Session: session-1") {
		t.Fatal(suspend)
	}
}

func TestWorkingSessionSuspensionConfirmsAndCancelIsSafe(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	started := uint64(1)
	mutableSessionDetail(dashboard, "session-1").currentTurnStartedAt = &started
	setWorking(dashboard, "session-1")
	requireDashboardAction(t, chord(dashboard, CommandIdSuspendSession), DashboardActionSuspendPreflight{SessionID: "session-1"})
	requireDashboardAction(t, dashboard.ApplySuspendPreflight(suspendPreflight("session-1")), DashboardActionNone{})
	if dialog, ok := dashboard.mode.(ModeConfirm); !ok {
		t.Fatalf("mode %#v", dashboard.mode)
	} else if _, ok := dialog.confirmation.(ConfirmationSuspendSession); !ok {
		t.Fatalf("confirmation %#v", dialog.confirmation)
	}
	// Enter initially activates Cancel.
	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionNone{})
	if !isDashboardMode(dashboard) {
		t.Fatalf("mode %#v", dashboard.mode)
	}
	requireDashboardAction(t, chord(dashboard, CommandIdSuspendSession), DashboardActionSuspendPreflight{SessionID: "session-1"})
	dashboard.ApplySuspendPreflight(suspendPreflight("session-1"))
	dashboard.HandleKey(key(tea.KeyRight))
	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionSuspend{
		SessionID: "session-1",
		Consent:   core.SuspendConsent{},
	})
}

// stoppedByTheParentsSuspend is the records a snapshot brings once the
// parent's suspend has stopped its one sub-agent: the parent is closing and
// the child is gone.
func stoppedByTheParentsSuspend(dashboard *DashboardState, parent string) core.StateState {
	state := *dashboard.state.Get()
	state.Sessions.GetMut(parent).State = core.SessionStateClosing
	state.Sessions.Remove("child-session")
	state.Subagents.Remove("child-session")
	return state
}

func requireStoppedBySuspend(t *testing.T, got StoppedBySuspend, want StoppedBySuspend) {
	t.Helper()
	if reflectSnapshot(got) != reflectSnapshot(want) {
		t.Fatalf("stopped by suspend %#v, want %#v", got, want)
	}
}

// TestASuspendThatStopsTheOpenSubAgentGoesBackToItsParent: R15-1: a suspend
// stopped the sub-agent whose conversation was open in its parent's
// Sub-agents view. The view fell to "No sessions yet" and the footer said
// "Could not save draft and read status for <child>: unknown session". It
// goes back to the parent's scope with the parent selected, and says what
// happened.
// Hard-won: 6e2e36c: suspending a parent lost the open child’s draft and read state.
func TestASuspendThatStopsTheOpenSubAgentGoesBackToItsParent(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	dashboard.FocusSessions()
	dashboard.HandleKey(key('I'))
	dashboard.HandleKey(key('/'))
	typeKeys(dashboard, "session")
	dashboard.HandleKey(key(tea.KeyEnter))
	topFilter := dashboard.sessionsFilterLabel()
	dashboard.OpenSubagentWorkspace(parent)
	dashboard.HandleKey(key('a'))
	if !noSessionsFilter(dashboard) {
		t.Fatal("All clears the child filter")
	}
	dashboard.SetCurrentSession(optionalText("child-session"))
	if !selectedSessionIs(dashboard, "child-session") {
		t.Fatal("the child is selected")
	}
	title := dashboard.state.Get().Sessions.GetShared("child-session").ListedTitle()

	dashboard.SetState(stoppedByTheParentsSuspend(dashboard, parent))

	if _, ok := dashboard.SubagentParentID(); ok {
		t.Fatal("the view went back to the top level")
	}
	if label := dashboard.sessionsFilterLabel(); label != topFilter {
		t.Fatalf("label %q", label)
	}
	if !selectedSessionIs(dashboard, parent) {
		t.Fatal("the parent is selected")
	}
	if notice, ok := dashboard.Notice(); !ok || notice != fmt.Sprintf("Sub-agent \"%s\" was stopped by the suspend", title) {
		t.Fatalf("notice %q", notice)
	}
	terminal := newRatatuiTestTerminal(60, 12)
	terminal.draw(func(frame *chat.Frame) {
		layout := newSessionsLayout(dashboard, frame.Area().Dx()-2)
		renderSessions(frame, frame.Area(), dashboard, &layout)
	})
	rows := strings.Join(chat.BufferLines(terminal.buffer), "\n")
	if strings.Contains(rows, "No sessions yet") {
		t.Fatal(rows)
	}
	dashboard.OpenSubagentWorkspace(parent)
	if !noSessionsFilter(dashboard) {
		t.Fatal("child All survives automatic exit")
	}
}

// TestTheHostLearnsWhichConversationsASuspendTookAway: the host lets the
// stopped sub-agent's conversation go without saving a draft for a session
// that no longer exists, and opens the parent's.
// Hard-won: 6e2e36c: the host did not learn which child conversations the suspend retired.
func TestTheHostLearnsWhichConversationsASuspendTookAway(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	dashboard.OpenSubagentWorkspace(parent)

	dashboard.SetState(stoppedByTheParentsSuspend(dashboard, parent))

	requireStoppedBySuspend(t, dashboard.TakeStoppedBySuspend(), StoppedBySuspend{
		Sessions: []string{"child-session"},
		Reopen:   &parent,
	})
	requireStoppedBySuspend(t, dashboard.TakeStoppedBySuspend(), StoppedBySuspend{})
}

// TestAParentAndSubAgentLeavingTogetherAfterASuspendIsStillReported: the
// feed carries only live sessions, so a parent whose suspend finished can
// leave in the same frame as the sub-agent it stopped. That is still the
// suspend's doing, and there is no parent conversation left to reopen.
func TestAParentAndSubAgentLeavingTogetherAfterASuspendIsStillReported(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	dashboard.OpenSubagentWorkspace(parent)
	dashboard.SetCurrentSession(optionalText("child-session"))
	closing := *dashboard.state.Get()
	closing.Sessions.GetMut(parent).State = core.SessionStateClosing
	dashboard.SetState(closing)
	requireStoppedBySuspend(t, dashboard.TakeStoppedBySuspend(), StoppedBySuspend{})

	gone := *dashboard.state.Get()
	gone.Sessions.Remove(parent)
	gone.Sessions.Remove("child-session")
	gone.Subagents.Remove("child-session")
	dashboard.SetState(gone)

	requireStoppedBySuspend(t, dashboard.TakeStoppedBySuspend(), StoppedBySuspend{
		Sessions: []string{"child-session"},
		Reopen:   nil,
	})
	if notice, ok := dashboard.Notice(); !ok || !strings.Contains(notice, "was stopped by the suspend") {
		t.Fatalf("notice %q", notice)
	}
}

// TestAPinnedSessionThatLeavesTheFeedBySuspendingSaysWhyItsPaneEmptied: a
// suspended session leaves the feed instead of staying as a stopped record.
// A pinned pane that showed it still says why it emptied.
func TestAPinnedSessionThatLeavesTheFeedBySuspendingSaysWhyItsPaneEmptied(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	pinned := runningSession()
	pinned.ID = "session-2"
	state := *dashboard.state.Get()
	state.Sessions.Insert(pinned.ID, pinned)
	dashboard.SetState(state)
	pane := dashboard.BrowsePane()
	if _, ok := dashboard.SplitFocusedPane(chat.DirectionHorizontal, nil); !ok {
		t.Fatal("the split has room")
	}
	dashboard.SetPaneSession(pane, optionalText("session-2"))
	if pane == dashboard.BrowsePane() {
		t.Fatal("the pinned pane is not Browse")
	}
	dashboard.BeginSessionOperationAtWithID("session-2", SessionOperationKindSuspending, nil, 0, nil, false)

	next := *dashboard.state.Get()
	next.Sessions.Remove("session-2")
	dashboard.SetState(next)
	dashboard.FinishSessionOperation("session-2")

	if !paneSessionIs(dashboard, pane, "") {
		t.Fatal("the pane emptied")
	}
	if notice, ok := dashboard.Notice(); !ok || !strings.Contains(notice, "is suspended, so it was unpinned from its pane") {
		t.Fatalf("notice %q", notice)
	}
}

// TestASubAgentRemovedWhileItsParentRunsIsNotReportedAsSuspended: a
// sub-agent that leaves while its parent runs on, as a destroy removes one,
// was not stopped by a suspend: the view and the footer stay as they were.
func TestASubAgentRemovedWhileItsParentRunsIsNotReportedAsSuspended(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	dashboard.OpenSubagentWorkspace(parent)
	state := *dashboard.state.Get()
	state.Sessions.Remove("child-session")
	state.Subagents.Remove("child-session")

	dashboard.SetState(state)

	if id, ok := dashboard.SubagentParentID(); !ok || id != parent {
		t.Fatalf("parent %q", id)
	}
	if notice, ok := dashboard.Notice(); ok {
		t.Fatalf("notice %q", notice)
	}
	requireStoppedBySuspend(t, dashboard.TakeStoppedBySuspend(), StoppedBySuspend{})
}

// TestASubAgentStoppedByItsParentsSuspendReadsStopping: R15-2: a sub-agent
// that its parent's suspend is stopping reads "Stopping" in its row and in
// its conversation's header, as the suspend dialog and the docs say. Before,
// both said "Destroying". A destroy a person asked for still says
// "Destroying".
// Hard-won: 6bfe27a: parent-driven child suspension was mislabeled as destruction.
func TestASubAgentStoppedByItsParentsSuspendReadsStopping(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	dashboard.OpenSubagentWorkspace(parent)
	dashboard.SetCurrentSession(optionalText("child-session"))

	dashboard.BeginSessionOperation("child-session", SessionOperationKindStopping, nil)
	screen := strings.Join(drawn(dashboard, 120, 40), "\n")
	if !strings.Contains(screen, "Transition · Stopping") || strings.Contains(screen, "Destroying") {
		t.Fatal(screen)
	}

	dashboard.FinishSessionOperation("child-session")
	dashboard.BeginSessionOperation("child-session", SessionOperationKindDestroying, nil)
	screen = strings.Join(drawn(dashboard, 120, 40), "\n")
	if !strings.Contains(screen, "Transition · Destroying") {
		t.Fatal(screen)
	}
}

// TestTheSuspendConfirmationNamesThreeWorkingSubAgentsAndCountsTheRest:
// R15-4: the suspend confirmation names the sub-agents still at their task,
// by listed title, up to three, and counts the rest.
// Hard-won: 1349c76: suspend confirmation counted active children without naming them.
func TestTheSuspendConfirmationNamesThreeWorkingSubAgentsAndCountsTheRest(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	mutableSession(dashboard, parent).ProjectDirectory = optionalText("/srv/project")
	template, _ := dashboard.state.Get().Sessions.Get("child-session")
	relation, _ := dashboard.state.Get().Subagents.Get("child-session")
	state := *dashboard.state.Get()
	state.Sessions.Remove("child-session")
	state.Subagents.Remove("child-session")
	for position, title := range []string{"Alpha", "Bravo", "Charlie", "Delta", "Echo"} {
		child := template
		child.ID = fmt.Sprintf("child-%d", position)
		child.SessionTitleOverride = optionalText(title)
		child.CreatedAt = fmt.Sprintf("2026-09-0%dT00:00:00Z", position+1)
		childRelation := relation
		childRelation.ChildSessionID = child.ID
		state.Subagents.Insert(child.ID, childRelation)
		state.Sessions.Insert(child.ID, child)
	}
	dashboard.SetState(state)
	for position := range 5 {
		setWorking(dashboard, fmt.Sprintf("child-%d", position))
	}
	dashboard.FocusSessions()
	dashboard.SelectActiveSession(parent)

	requireDashboardAction(t, dashboard.DispatchCommand(CommandIdSuspendSession), DashboardActionSuspendPreflight{SessionID: parent})
	dashboard.ApplySuspendPreflight(suspendPreflight(parent))
	dialog := strings.Join(drawn(dashboard, 160, 40), " ")
	if !strings.Contains(dialog, "Sub-agents \"Alpha\", \"Bravo\", \"Charlie\" and 2 more have not handed back;") {
		t.Fatal(dialog)
	}
}

func TestLiveRenderMembershipAndRecordUpdatesDoNotRevisitHistory(t *testing.T) {
	for _, count := range []int{100, 10_000, 100_000} {
		active := runningSession()
		active.ID = "active"
		dashboard := dashboardWithSession(active)
		state := *dashboard.state.Get()
		for n := range count {
			record := active
			record.ID = fmt.Sprintf("history-%d", n)
			record.State = core.SessionStateStopped
			state.Sessions.Insert(record.ID, record)
		}
		dashboard.SetState(state)
		if len(dashboard.orderedSessions()) != 1 {
			t.Fatalf("history size %d: one live row", count)
		}
		dashboard.rowIndex.visits = 0
		state.Sessions.GetMut("active").Title = "renamed"
		dashboard.SetState(state)
		if title := dashboard.orderedSessions()[0].Title; title != "renamed" {
			t.Fatalf("title %q", title)
		}
		dashboard.AttentionQueue()
		if visits := dashboard.rowIndex.visits; visits != 1 {
			t.Fatalf("history size %d: %d visits", count, visits)
		}
	}
}

func TestDurableUpdatesDoNotScanUnchangedNativePresentationRows(t *testing.T) {
	for _, count := range []int{100, 10_000, 100_000} {
		parent := runningSession()
		dashboard := dashboardWithSession(parent)
		durable := *dashboard.state.Get()
		views := make([]core.NativeAgentView, 0, count)
		for n := range count {
			views = append(views, nativeAgentView(core.NativeAgent{
				OwnerSessionID: parent.ID,
				SessionID:      fmt.Sprintf("child-%d", n),
				Name:           fmt.Sprintf("Child %d", n),
				State:          core.NativeAgentStateCompleted,
				Availability:   core.NativeAgentAvailabilityUnknown,
			}))
		}
		dashboard.SetNativeAgents(views)
		dashboard.SetState(durable)
		dashboard.reconciliationVisits = 0
		durable.Sessions.GetMut(parent.ID).Title = "Renamed"
		dashboard.SetState(durable)
		if dashboard.reconciliationVisits > 2 {
			t.Fatalf("native history size %d: %d visits", count, dashboard.reconciliationVisits)
		}
		if subagents := dashboard.SubagentCountFor(parent.ID); subagents != count {
			t.Fatalf("sub-agent count %d", subagents)
		}
		if working := dashboard.WorkingSubagentCountFor(parent.ID); working != 0 {
			t.Fatalf("working count %d", working)
		}
		if dashboard.reconciliationVisits > 2 {
			t.Fatalf("working count visited native history %d: %d visits", count, dashboard.reconciliationVisits)
		}
	}
}

// TestASubagentTheWorkerReportsAsRunningCountsAsWorkingForItsParent: I2-7:
// the composer footer's working count stayed 0 for a child's whole turn. The
// count must come from the facts the worker reports for the child, which is
// all a dashboard holds for a child nobody has opened, and it must agree with
// the child's Sessions row.
// Hard-won: 6b8110e: worker turn reports left the parent’s working count stale at zero.
func TestASubagentTheWorkerReportsAsRunningCountsAsWorkingForItsParent(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	if working := dashboard.WorkingSubagentCountFor(parent); working != 0 {
		t.Fatalf("working %d", working)
	}

	setWorking(dashboard, "child-session")
	if dashboard.AttentionLevel("child-session") != AttentionLevelWorking {
		t.Fatal("the child's own row reads it as working")
	}
	if working := dashboard.WorkingSubagentCountFor(parent); working != 1 {
		t.Fatalf("working %d", working)
	}
}

func TestStartingSessionClipboardShortcutsAndEmptyPasteUseTheComposerPipeline(t *testing.T) {
	// Nothing here waits for the spawned native reader, so it reports later
	// or never. This tests event routing and its immediate in-flight UI, not
	// a particular clipboard provider.
	for _, terminalPaste := range []bool{false, true} {
		dashboard := dashboardWithSession(runningSession())
		dashboard.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
		dashboard.FocusPrompt()
		if terminalPaste {
			dashboard.HandleEventResult(tea.PasteMsg{})
		} else {
			requireDashboardAction(t, dashboard.HandleKey(tea.KeyPressMsg{Code: 'v', Mod: tea.ModCtrl}), DashboardActionNone{},
				"the dashboard text-only reader must not own this paste")
		}
		standby := requireStandby(t, dashboard, "session-1")
		if notice, ok := standby.Notice(); !ok || notice != "Reading clipboard…" {
			t.Fatalf("notice %q", notice)
		}
		if standby.Draft() != "" {
			t.Fatalf("draft %q", standby.Draft())
		}
	}
}

func TestADashboardClipboardReplyCannotFollowFocusIntoAStartingSession(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	target := dashboard.PasteTarget()
	dashboard.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
	dashboard.FocusPrompt()
	dashboard.standbyPromptMut("session-1")
	if dashboard.ApplyClipboardText(&target, "belongs to the old field") {
		t.Fatal("a stale target pastes nothing")
	}
	if draft := requireStandby(t, dashboard, "session-1").Draft(); draft != "" {
		t.Fatalf("draft %q", draft)
	}
	dashboard.HandleKey(key('n'))
	target = dashboard.PasteTarget()
	dashboard.HandleKey(key('e'))
	if dashboard.ApplyClipboardText(&target, "stale read") {
		t.Fatal("an edited field rejects a stale read")
	}
	if draft := requireStandby(t, dashboard, "session-1").Draft(); draft != "ne" {
		t.Fatalf("draft %q", draft)
	}
}

func TestPastedImagePathsStageInAStartingSessionWithoutAnAttachmentStore(t *testing.T) {
	// Nothing waits here: attachment installation remains queued.
	dashboard := dashboardWithSession(runningSession())
	dashboard.BeginSessionOperation("session-1", SessionOperationKindLaunching, nil)
	dashboard.FocusPrompt()
	path := "/shots/before-start.png"
	if runtime.GOOS == "windows" {
		path = `C:\shots\before-start.png`
	}
	dashboard.HandleEventResult(tea.PasteMsg{Content: path})
	draft := requireStandby(t, dashboard, "session-1").Draft()
	payload, err := chat.PromptPayloadDecodeDraft(draft)
	if err != nil {
		t.Fatal(err)
	}
	if payload.Text != "[image 1]" {
		t.Fatalf("payload text %q", payload.Text)
	}
	if payload.Images[0].Image.DataBase64 != "" || payload.Images[0].Image.Reference != nil {
		t.Fatalf("image %#v", payload.Images[0].Image)
	}
	requireDashboardAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionNone{})
	if got := requireStandby(t, dashboard, "session-1").Draft(); got != draft {
		t.Fatalf("Enter must wait for image installation before durable enqueue: %q", got)
	}
	notice, _ := requireStandby(t, dashboard, "session-1").Notice()
	if !strings.Contains(notice, "Preparing images") {
		t.Fatalf("notice %q", notice)
	}
}

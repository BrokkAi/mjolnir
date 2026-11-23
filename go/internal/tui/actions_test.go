// Port of mj-tui/src/actions.rs tests.

package tui

import (
	"slices"
	"testing"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// A mis-hit key once stopped a live session, so no command that starts a
// session transition may claim one, either as a pane key or as a default
// binding. They stay reachable from the palette, the row's ⋯ menu, and an
// explicit `[keys]` entry.
// Hard-won: 7b69480385: a transition-key mis-hit stopped a session and stranded its close.
func TestSessionTransitionCommandsBindNoKey(t *testing.T) {
	defaults := core.DefaultKeybinds()
	for _, id := range []CommandId{
		CommandIdRestartSession,
		CommandIdMoveSession,
		CommandIdDestroySession,
	} {
		spec := spec(id)
		if len(spec.paneKeys) != 0 {
			t.Fatalf("%v must not bind a pane key", id)
		}
		if spec.action == nil {
			t.Fatalf("%v: a session command is bindable", id)
		}
		if bindings := defaults.Bindings(*spec.action); len(bindings) != 0 {
			t.Fatalf("%v must not carry a default binding: %v", id, bindings)
		}
	}
}

// actionsOperation is operation(kind, nil) as the dashboard stores it.
func actionsOperation(kind SessionOperationKind) *SessionOperationDisplay {
	display := operation(kind, nil)
	return &display
}

// A close that failed part-way leaves a durable Closing/Destroying record.
// Stop is how the person retries it, so it stays available even though
// every other session command is blocked on the failure.
// Hard-won: 7b69480385: a failed close left a Destroying session without a retry route.
func TestStopRetriesACloseThatFailedPartWay(t *testing.T) {
	session := runningSession()
	session.State = core.SessionStateDestroying
	session.LastError = wzPtr("archive unavailable")
	dashboard := dashboardWithSession(session)
	dashboard.FocusSessions()
	if availability := spec(CommandIdSuspendSession).available(dashboard); availability != (AvailabilityReady{}) {
		t.Fatalf("Stop %#v, want Ready", availability)
	}

	// An ordinary running session with an operation in flight is not a
	// failed close, and Stop stays blocked behind that operation.
	dashboard = dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	(*dashboard.sessionOperations.Mut())["session-1"] = actionsOperation(SessionOperationKindLaunching)
	for _, command := range []CommandId{
		CommandIdSuspendSession,
		CommandIdRestartSession,
		CommandIdMoveSession,
	} {
		want := AvailabilityBlocked{Reason: "an operation is in progress"}
		if availability := spec(command).available(dashboard); availability != want {
			t.Fatalf("%v: %#v, want %#v", command, availability, want)
		}
	}
}

// A-17: the Sub-agents pane opened only by a mouse click on the prompt's
// lower border. It is a registry command now, so the palette, a default
// chord, and the footer all reach it.
// Hard-won: 85c59fc968: the existing Sub-agents pane had no keyboard route.
func TestSubAgentsOpenFromAChordAndAreListedInThePalette(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	dashboard.FocusSessions()
	if labels := dashboard.keyLabels(CommandIdOpenSubagents); !slices.Equal(labels, []string{"ctrl+b shift+a"}) {
		t.Fatalf("labels %q", labels)
	}
	if availability := spec(CommandIdOpenSubagents).available(dashboard); availability != (AvailabilityReady{}) {
		t.Fatalf("availability %#v, want Ready", availability)
	}
	if hiddenFromPalette(CommandIdOpenSubagents) {
		t.Fatal("the palette lists Sub-agents")
	}
	wzContains(t, spec(CommandIdOpenSubagents).label, "Sub-agents")
	footer := combinedFooterText(dashboard, 400)
	wzContains(t, footer, "shift+a sub-agents")
	wzAction(t, dashboard.DispatchCommand(CommandIdOpenSubagents), DashboardActionOpenSubagents{ParentID: parent})

	// With the filter cleared, Esc goes back to the parent, as the X on the
	// workspace strip does.
	dashboard.OpenSubagentWorkspace(parent)
	dashboard.HandleKey(key('a'))
	wzAction(t, dashboard.HandleKey(key(tea.KeyEscape)), DashboardActionExitSubagentWorkspace{})

	// A session without sub-agents keeps the command listed, greyed with the
	// reason, so a search for it still finds it.
	dashboard = dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	if _, blocked := spec(CommandIdOpenSubagents).available(dashboard).(AvailabilityBlocked); !blocked {
		t.Fatal("without sub-agents the command is listed with its reason")
	}
	footer = combinedFooterText(dashboard, 400)
	wzNotContains(t, footer, "sub-agents")
}

// A-8 / B-1: the row's ⋯ menu had no keyboard route. "Session actions…"
// opens the same menu from a chord, from `.` on the Sessions pane, and from
// the palette, and the Sessions footer names it.
// Hard-won: ddd30e47d5: the existing session row menu had no keyboard route.
func TestSessionActionsOpenTheRowMenuFromTheKeyboard(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	if labels := dashboard.keyLabels(CommandIdSessionActions); !slices.Equal(labels, []string{".", "ctrl+b ."}) {
		t.Fatalf("labels %q", labels)
	}
	if hiddenFromPalette(CommandIdSessionActions) {
		t.Fatal("the palette lists Session actions")
	}
	footer := combinedFooterText(dashboard, 400)
	wzContains(t, footer, ". actions")

	dashboard.HandleKey(key('.'))
	menu, ok := dashboard.mode.(ModePalette)
	if !ok {
		t.Fatal("the row menu opens")
	}
	fromKey := slices.Clone(menu.entries)
	dashboard.mode = ModeDashboard{}
	// The mouse's ⋯ opens the same menu.
	dashboard.beginSessionPalette()
	menu, ok = dashboard.mode.(ModePalette)
	if !ok {
		t.Fatal("the row menu opens")
	}
	if !wzEqual(fromKey, menu.entries) {
		t.Fatal("the key and the mouse open the same menu")
	}

	// From the composer the chord opens the menu for the conversation's
	// session.
	dashboard.mode = ModeDashboard{}
	dashboard.FocusPrompt()
	dashboard.DispatchCommand(CommandIdSessionActions)
	if _, ok := dashboard.mode.(ModePalette); !ok {
		t.Fatalf("mode %T, want the row menu", dashboard.mode)
	}
}

// Interrupt all asks first, then ends the turns that are running when it is
// confirmed: the parent's and its working sub-agent's, through the daemon,
// whether or not those conversations are open here.
func TestInterruptAllConfirmsThenInterruptsParentAndWorkingSubagents(t *testing.T) {
	dashboard, parent := dashboardWithOneSubagent()
	dashboard.FocusSessions()
	(*dashboard.sessionDetails.Mut())["child-session"] = &SessionDetail{}
	want := AvailabilityBlocked{Reason: "no turn is running here or in its sub-agents"}
	if availability := spec(CommandIdInterruptAll).available(dashboard); availability != want {
		t.Fatalf("availability %#v, want %#v", availability, want)
	}
	setWorking(dashboard, "child-session")
	wzAction(t, dashboard.DispatchCommand(CommandIdInterruptAll), DashboardActionNone{})
	dialog, ok := dashboard.mode.(ModeConfirm)
	if !ok {
		t.Fatal("Interrupt all asks first")
	}
	if !wzEqual(dialog.confirmation, Confirmation(ConfirmationInterruptAll{
		SessionID:        parent,
		ParentRunning:    false,
		SubagentsRunning: 1,
	})) {
		t.Fatalf("confirmation %#v", dialog.confirmation)
	}

	// The parent's turn starts while the dialog is open; confirming
	// interrupts what is running then.
	setWorking(dashboard, parent)
	action := dashboard.HandleKey(key('i'))
	wzAction(t, action, DashboardActionInterruptAll{SessionID: parent})
}

// B-16: the terminal said "Interrupt turn" nowhere. The command is in the
// palette at all times, greyed with the reason while no turn runs, and it
// runs only while the session is working.
// Hard-won: f5821dce22: users could not find Interrupt turn in the terminal controls.
func TestInterruptTurnIsListedAlwaysAndRunsOnlyDuringATurn(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	if label := spec(CommandIdInterruptTurn).label; label != "Interrupt turn" {
		t.Fatalf("label %q", label)
	}
	if labels := dashboard.keyLabels(CommandIdInterruptTurn); !slices.Equal(labels, []string{"ctrl+b i"}) {
		t.Fatalf("labels %q", labels)
	}
	if hiddenFromPalette(CommandIdInterruptTurn) {
		t.Fatal("the palette lists Interrupt turn")
	}
	want := AvailabilityBlocked{Reason: "no turn is running"}
	if availability := spec(CommandIdInterruptTurn).available(dashboard); availability != want {
		t.Fatalf("availability %#v, want %#v", availability, want)
	}
	wzAction(t, dashboard.DispatchCommand(CommandIdInterruptTurn), DashboardActionNone{})

	setWorking(dashboard, "session-1")
	if availability := spec(CommandIdInterruptTurn).available(dashboard); availability != (AvailabilityReady{}) {
		t.Fatalf("availability %#v, want Ready", availability)
	}
	wzAction(t, dashboard.DispatchCommand(CommandIdInterruptTurn), DashboardActionInterruptTurn{SessionID: "session-1"})
}

// Hard-won: f1947384e: the terminal Interrupt action omitted a standalone user shell.
func TestInterruptTurnCanDispatchForAStandaloneUserShell(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.SetCurrentSession(nil)
	dashboard.FocusSessions()
	dashboard.commandSessionOverride = wzPtr("session-1")
	verdict := dashboard.verdict("session-1")
	if verdict.Interrupt {
		t.Fatal("no turn is running")
	}
	verdict.InterruptShell = true
	dashboard.sessionVerdicts.Mut().Insert("session-1", verdict)
	want := AvailabilityBlocked{Reason: "open the conversation to interrupt its shell"}
	if availability := interruptAvailable(dashboard); availability != want {
		t.Fatalf("availability %#v, want %#v", availability, want)
	}
	wzAction(t, dashboard.DispatchCommand(CommandIdInterruptTurn), DashboardActionNone{})
	dashboard.commandSessionOverride = nil
	dashboard.SetPaneSession(dashboard.BrowsePane(), wzPtr("session-1"))
	wzAction(t, dashboard.DispatchCommand(CommandIdInterruptTurn), DashboardActionInterruptTurn{SessionID: "session-1"})
}

func TestSpinnerSelectionWaitsForTheCurrentSaveBeforeAcceptingAnother(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	first := dashboard.DispatchCommand(CommandIdCycleSpinner)
	if save, ok := first.(DashboardActionSaveSpinnerStyle); !ok || save.Style != core.SpinnerStylePulse {
		t.Fatalf("first %s, want saving pulse", wzDebug(first))
	}
	if _, blocked := spinnerAvailable(dashboard).(AvailabilityBlocked); !blocked {
		t.Fatal("a second selection waits for the save")
	}
	if _, none := dashboard.DispatchCommand(CommandIdCycleSpinner).(DashboardActionNone); !none {
		t.Fatal("a second selection does nothing while the first saves")
	}

	dashboard.config.Mut().Spinner = core.SpinnerStylePulse
	dashboard.FinishSpinnerStyleSave()
	if availability := spinnerAvailable(dashboard); availability != (AvailabilityReady{}) {
		t.Fatalf("availability %#v, want Ready", availability)
	}
	if save, ok := dashboard.DispatchCommand(CommandIdCycleSpinner).(DashboardActionSaveSpinnerStyle); !ok || save.Style != core.SpinnerStyleWave {
		t.Fatal("the next selection saves wave")
	}
}

// Two commands answering the same key in the same place would make the
// registry order, rather than the user's intent, decide what happens.
func TestNoTwoCommandsClaimTheSameKeyInOnePane(t *testing.T) {
	type claim struct {
		code      rune
		modifiers tea.KeyMod
	}
	for _, focus := range []Focus{FocusSessions, FocusTargets, FocusQuota, FocusNotifications} {
		var seen []claim
		for i := range commands {
			entry := &commands[i]
			if entry.scope == ScopeSetup || !scopeApplies(entry.scope, focus) {
				continue
			}
			for _, hint := range entry.paneKeys {
				pressed := claim{hint.code, hint.modifiers}
				if slices.Contains(seen, pressed) {
					t.Fatalf("%q is claimed twice at %v", string(hint.code), focus)
				}
				seen = append(seen, pressed)
			}
		}
	}
}

func TestForceDestroyNeedsASelectedSessionAndSurvivesInFlightOperations(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	if !slices.Contains(available(dashboard, nil), CommandIdDestroySession) {
		t.Fatal("a selected session can be destroyed")
	}

	(*dashboard.sessionOperations.Mut())["session-1"] = actionsOperation(SessionOperationKindLaunching)
	if !slices.Contains(available(dashboard, nil), CommandIdDestroySession) {
		t.Fatal("force destruction exists to preempt a wedged operation")
	}
}

func TestPreparedMoveIsRetainedForTheExplicitConfirmationSubmit(t *testing.T) {
	dashboard := dashboardWithSession(runningSession())
	dashboard.FocusSessions()
	dashboard.DispatchCommand(CommandIdMoveSession)
	wzAction(t, dashboard.HandleKey(key(tea.KeyEnter)), DashboardActionNone{})
	action := dashboard.HandleKey(key(tea.KeyEnter))
	move, ok := action.(DashboardActionMoveSession)
	if !ok || move.PreparationRequestID == nil {
		t.Fatalf("entering move review should request preparation: %s", wzDebug(action))
	}
	preparation := movePreparation()
	if !dashboard.ApplyMovePreparation(*move.PreparationRequestID, preparation) {
		t.Fatal("the preparation applies")
	}
	mode, ok := dashboard.mode.(ModeResume)
	if !ok {
		t.Fatal("move remains in its confirmation wizard")
	}
	if mode.preparation == nil || !wzEqual(*mode.preparation, preparation) {
		t.Fatal("the wizard retains the preparation")
	}
	if mode.preparing {
		t.Fatal("the preparation finished")
	}
	if executed, ok := dashboard.HandleKey(key(tea.KeyEnter)).(DashboardActionMoveSession); !ok || executed.PreparationRequestID != nil {
		t.Fatal("the confirmation executes the prepared move")
	}
}

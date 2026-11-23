// Port of the tests in mj-client/src/usage_format.rs.

package client

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/worker"
)

// assertRestGolden compares output with testdata/golden/<name>.txt, the copy
// of mj-client/tests/golden/<name>.txt. Go output never rewrites it.
func assertRestGolden(t *testing.T, name, actual string) {
	t.Helper()
	path := filepath.Join("testdata", "golden", name+".txt")
	expected, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read golden %s: %v", path, err)
	}
	want := strings.TrimSuffix(string(expected), "\n")
	got := strings.TrimSuffix(actual, "\n")
	if want == got {
		return
	}
	wantLines, gotLines := strings.Split(want, "\n"), strings.Split(got, "\n")
	for i := 0; i < max(len(wantLines), len(gotLines)); i++ {
		var w, g string
		if i < len(wantLines) {
			w = wantLines[i]
		}
		if i < len(gotLines) {
			g = gotLines[i]
		}
		if w != g {
			t.Fatalf("golden %s differs at line %d:\nwant %q\ngot  %q\n\nfull output:\n%s", path, i+1, w, g, got)
		}
	}
}

func usagePtr[T any](value T) *T { return &value }

func TestQuotaRecoveryStatusShowsWaitAndUnknownDeadline(t *testing.T) {
	activity := SessionActivity{QuotaRecovery: &core.QuotaRecovery{
		UserCommandID:      "user-request",
		CompletedCommandID: "completed",
		ProfileID:          "test",
		ResetAtMs:          usagePtr(int64(60_000)),
		RetryAtMs:          usagePtr(int64(120_000)),
		Notice:             "waiting",
	}}
	if got := activity.DisplayClock(60, nil, nil, false); got != "Quota limit · resumes in 1m00s" {
		t.Errorf("display clock %q", got)
	}
	activity.QuotaRecovery.RetryAtMs = nil
	if got := FormatActivityClock(60, nil, &activity); got != "Quota limit · reset time unknown" {
		t.Errorf("activity clock %q", got)
	}
}

func TestALiveSdkStepProvesWorkButAnIdleStepClockDoesNot(t *testing.T) {
	relay, err := worker.DurableRelayOpen(t.TempDir(), "step-session", "test")
	if err != nil {
		t.Fatal(err)
	}
	state := relay.OperationalState()
	// This test edits the snapshot's fields by hand, so the answer the
	// worker published with it no longer describes them. Clear it and let
	// the shared classifier read the edited facts, which is exactly what
	// happens with a worker too old to publish an answer at all.
	state.Activity = core.ActivityState{}
	state.Execution = core.RelayExecutionStateRunning
	state.CurrentStepStartedAtMs = usagePtr(int64(50_000))
	activity := SessionActivityOf(&state)
	if !activity.IsWorking(nil, false) {
		t.Error("a live step is not working")
	}
	if got := activity.DisplayClock(60, nil, nil, false); got != "Working 10s" {
		t.Errorf("display clock %q", got)
	}
	state.Execution = core.RelayExecutionStateIdle
	if idle := SessionActivityOf(&state); idle.IsWorking(nil, false) {
		t.Error("an idle step clock is working")
	}
	state.Execution = core.RelayExecutionStateRunning
	state.CurrentStepStartedAtMs = nil
	if running := SessionActivityOf(&state); running.IsWorking(nil, false) {
		t.Error("a running relay without a step is working")
	}
}

func usageBackground(startedAtMs int64, command string) SessionActivity {
	return SessionActivity{BackgroundCommands: []core.BackgroundCommand{{
		ID:          "test-background",
		StartedAtMs: startedAtMs,
		Command:     command,
	}}}
}

func TestAStepThatPredatesItsTurnReadsFromTheTurnStart(t *testing.T) {
	// The step clock belongs to the previous turn until the new turn's
	// first update lands; a row never claims a step older than its turn.
	got := FormatActivityColumns(20_000, usagePtr(uint64(19_900)), usagePtr(uint64(17_384_000)), &SessionActivity{})
	if !slices.Equal(got, []string{"Turn 1m40s", "Step 1m40s"}) {
		t.Errorf("columns %q", got)
	}
}

func TestStructuredActivityClampsStepsToTheirTurn(t *testing.T) {
	activity := SessionActivity{}
	details := activity.Details(usagePtr(int64(20_000_000)), usagePtr(int64(19_000_000)))
	if details.Kind != SessionActivityKindTurn {
		t.Errorf("kind %v", details.Kind)
	}
	if details.TurnStartedAtMs == nil || *details.TurnStartedAtMs != 20_000_000 {
		t.Errorf("turn %v", details.TurnStartedAtMs)
	}
	if details.StepStartedAtMs == nil || *details.StepStartedAtMs != 20_000_000 {
		t.Errorf("step %v", details.StepStartedAtMs)
	}
	details = activity.Details(usagePtr(int64(19_000_000)), usagePtr(int64(20_000_000)))
	if details.StepStartedAtMs == nil || *details.StepStartedAtMs != 20_000_000 {
		t.Errorf("later step %v", details.StepStartedAtMs)
	}
}

func TestStructuredActivityKeepsKnownIdleAndMissingIdleSinceDistinct(t *testing.T) {
	known := (&SessionActivity{
		Execution:   usagePtr(core.RelayExecutionStateIdle),
		IdleSinceMs: usagePtr(int64(19_000_000)),
	}).Details(nil, nil)
	if known.Kind != SessionActivityKindIdle || known.IdleSinceMs == nil || *known.IdleSinceMs != 19_000_000 {
		t.Errorf("known idle %+v", known)
	}
	oldWorker := (&SessionActivity{Execution: usagePtr(core.RelayExecutionStateIdle)}).Details(nil, nil)
	if oldWorker.Kind != SessionActivityKindIdle || oldWorker.IdleSinceMs != nil {
		t.Errorf("old worker %+v", oldWorker)
	}
}

func TestATurnHidesBackgroundWorkEvenBeforeTheProjectionCatchesUp(t *testing.T) {
	activity := usageBackground(17_384_000, "cargo test")
	activity.HarnessTurnStartedAtMs = usagePtr(int64(19_000_000))
	if got := FormatActivityColumns(20_000, nil, nil, &activity); !slices.Equal(got, []string{"Turn 16m40s", "Step 16m40s"}) {
		t.Errorf("a row reports the harness turn while the projection catches up: %q", got)
	}
	if got := FormatActivityClock(20_000, nil, &activity); got != "[Turn 16m40s]" {
		t.Errorf("clock %q", got)
	}
}

func TestIdleRequiresNoProjectedOrRelayActivity(t *testing.T) {
	activity := SessionActivity{}
	if !activity.IsIdle(nil) {
		t.Error("default is not idle")
	}
	if activity.IsIdle(usagePtr(uint64(20_000))) {
		t.Error("a projected turn is idle")
	}
	activity.HarnessTurnStartedAtMs = usagePtr(int64(19_000_000))
	if activity.IsIdle(nil) {
		t.Error("a harness turn is idle")
	}
	activity.HarnessTurnStartedAtMs = nil
	activity.ForegroundToolStartedAtMs = usagePtr(int64(19_000_000))
	if activity.IsIdle(nil) {
		t.Error("a foreground tool is idle")
	}
	activity.ForegroundToolStartedAtMs = nil
	activity.BackgroundCommands = usageBackground(19_000_000, "cargo test").BackgroundCommands
	if activity.IsIdle(nil) {
		t.Error("background work is idle")
	}
}

func TestInvalidActivityTimestampsStillReportWorkWithoutFabricatingAClock(t *testing.T) {
	foreground := SessionActivity{ForegroundToolStartedAtMs: usagePtr(int64(-1))}
	if foreground.IsIdle(nil) {
		t.Error("foreground is idle")
	}
	if got := FormatActivityColumns(20_000, nil, nil, &foreground); !slices.Equal(got, []string{"Step"}) {
		t.Errorf("foreground columns %q", got)
	}
	if got := FormatActivityClock(20_000, nil, &foreground); got != "[Step]" {
		t.Errorf("foreground clock %q", got)
	}

	background := usageBackground(-1, "cargo test")
	if background.IsIdle(nil) {
		t.Error("background is idle")
	}
	if got := FormatActivityColumns(20_000, nil, nil, &background); !slices.Equal(got, []string{"  BG"}) {
		t.Errorf("background columns %q", got)
	}
	if got := FormatActivityClock(20_000, nil, &background); got != "[BG]" {
		t.Errorf("background clock %q", got)
	}
}

// A running session's row shows how long the harness has been quiet, and
// only once that is worth saying.
//
// This is a report, not a warning. Mjolnir does not end a turn for silence
// (#1017), so the row's job is to let a person see "nothing for eleven
// minutes" and decide for themselves whether to cancel.
// Hard-won: 6e3a14fe2db0: #1017: a silent harness was invisible in the default session row
func TestAQuietRunningSessionGainsAQuietColumn(t *testing.T) {
	quiet := SessionActivity{
		PromptInFlight: true,
		Execution:      usagePtr(core.RelayExecutionStateRunning),
		ReportedState: &core.ActivityState{
			Kind:             core.ActivityStateTurn,
			StartedAtMs:      usagePtr(int64(19_000_000)),
			LastActivityAtMs: usagePtr(int64(19_340_000)),
		},
	}
	got := FormatActivityColumns(20_000, usagePtr(uint64(19_000)), nil, &quiet)
	if !slices.Equal(got, []string{"Turn 16m40s", "Step 16m40s", "Quiet 11m00s"}) {
		t.Errorf("quiet columns %q", got)
	}
	if clock := quiet.DisplayClock(20_000, usagePtr(uint64(19_000)), nil, true); !strings.HasSuffix(clock, "Q 11m00s") {
		t.Errorf("the detailed clock carries it too: %s", clock)
	}

	// A session that spoke a second ago is not quiet, and neither is an idle
	// one however long ago it last said anything.
	talking := quiet
	talking.ReportedState = &core.ActivityState{
		Kind:             core.ActivityStateTurn,
		StartedAtMs:      usagePtr(int64(19_000_000)),
		LastActivityAtMs: usagePtr(int64(19_999_000)),
	}
	if got := FormatActivityColumns(20_000, usagePtr(uint64(19_000)), nil, &talking); !slices.Equal(got, []string{"Turn 16m40s", "Step 16m40s"}) {
		t.Errorf("talking columns %q", got)
	}

	idle := SessionActivity{
		Execution:     usagePtr(core.RelayExecutionStateIdle),
		ReportedState: &core.ActivityState{Kind: core.ActivityStateIdle, SinceMs: usagePtr(int64(19_340_000))},
	}
	if _, ok := idle.quietSince(20_000); ok {
		t.Error("an idle session is quiet")
	}
}

func TestRelayExecutionAndUserShellsKeepActivityNonIdleWithoutTimestamps(t *testing.T) {
	running := SessionActivity{PromptInFlight: true, Execution: usagePtr(core.RelayExecutionStateRunning)}
	if running.IsIdle(nil) {
		t.Error("running is idle")
	}
	if got := FormatActivityColumns(20_000, nil, nil, &running); !slices.Equal(got, []string{"Turn", "Step"}) {
		t.Errorf("running columns %q", got)
	}
	if got := FormatActivityClock(20_000, nil, &running); got != "[Turn]" {
		t.Errorf("running clock %q", got)
	}

	for _, execution := range []core.RelayExecutionState{core.RelayExecutionStateClosing, core.RelayExecutionStateClosed} {
		lifecycle := SessionActivity{Execution: usagePtr(execution)}
		if lifecycle.IsIdle(nil) {
			t.Errorf("%v is idle", execution)
		}
		label := "Closing"
		if execution == core.RelayExecutionStateClosed {
			label = "Closed"
		}
		if got := FormatActivityColumns(20_000, nil, nil, &lifecycle); !slices.Equal(got, []string{label}) {
			t.Errorf("%s columns %q", label, got)
		}
		if got := FormatActivityClock(20_000, nil, &lifecycle); got != "["+label+"]" {
			t.Errorf("%s clock %q", label, got)
		}
	}

	shell := SessionActivity{ActiveUserShells: []core.ActiveUserShell{{
		CommandID:   "shell-1",
		Command:     "cargo test",
		CreatedAtMs: 20_000_000,
	}}}
	if shell.IsIdle(nil) {
		t.Error("a shell is idle")
	}
	if got := FormatActivityColumns(20_000, nil, nil, &shell); !slices.Equal(got, []string{"  BG"}) {
		t.Errorf("shell columns %q", got)
	}
	if got := FormatActivityClock(20_000, nil, &shell); got != "[BG]" {
		t.Errorf("shell clock %q", got)
	}
}

func TestActivityIndicatorsIgnoreStalePhasesAndQuestionsWithoutWork(t *testing.T) {
	activity := SessionActivity{Execution: usagePtr(core.RelayExecutionStateRunning)}
	if activity.IsWorking(nil, false) {
		t.Error("a stale running phase is working")
	}
	if got := activity.DisplayClock(60, nil, nil, false); got != "Idle" {
		t.Errorf("stale clock %q", got)
	}
	activity.PromptInFlight = true
	if !activity.IsWorking(nil, false) {
		t.Error("a prompt in flight is not working")
	}
	if got := activity.DisplayClock(60, nil, nil, false); got != "Working" {
		t.Errorf("working clock %q", got)
	}
	if activity.IsWorking(usagePtr(uint64(10)), true) {
		t.Error("a question without work is working")
	}
	activity.BackgroundCommands = usageBackground(20_000, "build").BackgroundCommands
	if !activity.IsWorking(usagePtr(uint64(10)), true) {
		t.Error("background work under a question is not working")
	}
	activity.Execution = usagePtr(core.RelayExecutionStateClosed)
	if activity.IsWorking(usagePtr(uint64(10)), false) {
		t.Error("a closed session is working")
	}
	if got := activity.DisplayClock(60, usagePtr(uint64(10)), nil, false); got != "Closed" {
		t.Errorf("closed clock %q", got)
	}
}

// Hard-won: b72adcc6f: hiding assessments on idle sessions also hid genuine pending checks.
func TestGoldenCliDashboardSessionActivity(t *testing.T) {
	var out strings.Builder
	row := func(label string, now uint64, turnStarted, stepStartedMs *uint64, activity *SessionActivity) {
		fmt.Fprintf(&out, "=== %s (1 session row) ===\n", label)
		fmt.Fprintf(&out, "wide activity: %s\n", strings.Join(FormatActivityColumns(now, turnStarted, stepStartedMs, activity), "  "))
		fmt.Fprintf(&out, "compact activity: %s\n", FormatActivityClock(now, turnStarted, activity))
	}

	expecting := SessionActivity{ReportedState: &core.ActivityState{Kind: core.ActivityStateExpecting, SinceMs: usagePtr(int64(1_000))}}
	row("expected continuation", 10, nil, nil, &expecting)
	fmt.Fprintf(&out, "idle: %t; working without turn: %t; working with turn: %t; kind: %v\n",
		expecting.IsIdle(nil), expecting.IsWorking(nil, false), expecting.IsWorking(usagePtr(uint64(1)), false),
		expecting.Details(nil, nil).Kind)
	fmt.Fprintf(&out, "chat title: %s\n", expecting.DisplayClock(10, nil, nil, true))

	relay, err := worker.DurableRelayOpen(t.TempDir(), "018f9dd2-a3b4-7c8d-9000-123456789abc", "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	relay.SetBackgroundWorkPolicy(worker.BackgroundWorkPolicyAirTasks)
	if err := relay.AirTaskChanged("root", core.AirTaskUpdateSpawned{
		TaskID:      "design-review",
		Description: "Design simplification and cleanup review",
		CanStop:     true,
	}); err != nil {
		t.Fatal(err)
	}
	state := relay.OperationalState()
	now := uint64(state.BackgroundCommands[0].StartedAtMs/1_000) + 60
	activity := SessionActivityOf(&state)
	row("Claude background agent active", now, nil, nil, &activity)
	fmt.Fprintf(&out, "idle: %t\n", activity.IsIdle(nil))
	if err := relay.AirTaskChanged("root", core.AirTaskUpdateFinished{TaskID: "design-review"}); err != nil {
		t.Fatal(err)
	}
	finishedState := relay.OperationalState()
	activity = SessionActivityOf(&finishedState)
	row("Claude background agent finished", now, nil, nil, &activity)
	fmt.Fprintf(&out, "idle: %t\n", activity.IsIdle(nil))

	snapshot := core.NewRelaySnapshot("assessment-session")
	snapshot.IdleSinceMs = usagePtr(int64(1_000))
	assessment := core.TurnAssessmentPending("completed-turn", 10, 1_000, core.CompletionEvidenceBounded("EndTurn", nil))
	for _, check := range []struct {
		label  string
		status core.Status
	}{
		{"idle with a pending assessment", core.StatusPending},
		{"idle with a failed assessment", core.StatusFailed},
	} {
		assessment.Status = check.status
		copied := assessment
		snapshot.Assessment = &copied
		encoded, err := json.Marshal(snapshot.OperationalState())
		if err != nil {
			t.Fatal(err)
		}
		var operational core.RelayOperationalState
		if err := json.Unmarshal(encoded, &operational); err != nil {
			t.Fatal(err)
		}
		activity := SessionActivityOf(&operational)
		row(check.label, 10, nil, nil, &activity)
		fmt.Fprintf(&out, "assessment unresolved: %t; idle: %t; chat title: %s\n",
			operational.RetryAssessmentPending, activity.IsIdle(nil), activity.DisplayClock(10, nil, nil, true))
	}

	out.WriteString("=== elapsed clock cells (11 values) ===\n")
	for _, cell := range []struct {
		seconds  uint64
		expected string
	}{
		{0, "0s"}, {36, "36s"}, {59, "59s"}, {60, "1m00s"}, {2_616, "43m36s"}, {3_599, "59m59s"},
		{3_600, "1h00m"}, {6_180, "1h43m"}, {86_399, "23h59m"}, {86_400, "1d00h"}, {183_600, "2d03h"},
	} {
		fmt.Fprintf(&out, "%ds -> %s\n", cell.seconds, FormatClock(cell.seconds))
		if got := FormatClock(cell.seconds); got != cell.expected {
			t.Errorf("FormatClock(%d) = %q, want %q", cell.seconds, got, cell.expected)
		}
	}
	out.WriteString("=== turn clock cells (3 states) ===\n")
	for _, cell := range []struct {
		now     uint64
		started *uint64
	}{{500, usagePtr(uint64(375))}, {400_000, usagePtr(uint64(1_000))}, {5_000, nil}} {
		fmt.Fprintf(&out, "%s\n", FormatTurnClock(cell.now, cell.started))
	}

	goal := SessionActivity{PursuingGoal: true}
	row("active goal between turns", 100, nil, nil, &goal)
	fmt.Fprintf(&out, "idle: %t; working: %t; kind: %v\n", goal.IsIdle(nil), goal.IsWorking(nil, false), goal.Details(nil, nil).Kind)

	foreground := usageBackground(17_384_000, "cargo test --old")
	foreground.ForegroundToolStartedAtMs = usagePtr(int64(19_900_000))
	row("foreground tool over older background task", 20_000, nil, nil, &foreground)

	idle := SessionActivity{}
	waiting := usageBackground(17_384_000, "cargo test")
	row("running turn and step", 20_000, usagePtr(uint64(17_384)), usagePtr(uint64(19_988_000)), &idle)
	row("background work", 20_000, nil, usagePtr(uint64(19_988_000)), &waiting)
	row("idle session", 20_000, nil, nil, &idle)
	row("turn with no reported step", 20_000, usagePtr(uint64(17_384)), nil, &idle)

	compact := usageBackground(20_000, "build")
	compact.ActivityTurnStartedAtMs = usagePtr(int64(10_000))
	out.WriteString("=== narrow activity clock (6 states) ===\n")
	for _, cell := range []struct {
		label           string
		now             uint64
		turn            *uint64
		turnFinished    *uint64
		includeTurnStep bool
		activity        SessionActivity
	}{
		{"turn and background", 60, usagePtr(uint64(10)), usagePtr(uint64(50_000)), false, compact},
		{"turn and background with both clocks", 60, usagePtr(uint64(10)), usagePtr(uint64(50_000)), true, compact},
		{"background only", 70, nil, nil, false, compact},
		{"background shorthand", 70, nil, nil, true, compact},
		{"newer turn", 70, usagePtr(uint64(65)), nil, false, compact},
		{"idle", 70, nil, nil, false, SessionActivity{}},
	} {
		fmt.Fprintf(&out, "%s: %s\n", cell.label, cell.activity.DisplayClock(cell.now, cell.turn, cell.turnFinished, cell.includeTurnStep))
	}

	tasks := usageBackground(20_000, "build")
	tasks.BackgroundCommands = append(tasks.BackgroundCommands, core.BackgroundCommand{
		ID: "second", StartedAtMs: 30_000, Command: "test",
	})
	out.WriteString("=== background task and shell counts (5 states) ===\n")
	fmt.Fprintf(&out, "%s\n", tasks.DisplayClock(70, nil, nil, false))
	tasks.ActiveUserShells = append(tasks.ActiveUserShells, core.ActiveUserShell{
		CommandID: "shell", Command: "watch", CreatedAtMs: 40_000, StartedAtMs: usagePtr(int64(40_000)),
	})
	fmt.Fprintf(&out, "%s\n", tasks.DisplayClock(70, nil, nil, false))
	tasks.BackgroundCommands = nil
	fmt.Fprintf(&out, "%s\n", tasks.DisplayClock(70, nil, nil, false))
	tasks.ForegroundToolStartedAtMs = usagePtr(int64(60_000))
	fmt.Fprintf(&out, "%s\n", tasks.DisplayClock(70, nil, nil, false))
	tasks.ForegroundToolStartedAtMs = nil
	tasks.ActiveUserShells = nil
	fmt.Fprintf(&out, "%s\n", tasks.DisplayClock(70, nil, nil, false))

	assertRestGolden(t, "cli-dashboard-session-activity", out.String())
}

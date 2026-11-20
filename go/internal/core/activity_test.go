// Port of mj-core/src/activity/tests.rs.

// State-transition tests for the shared activity mechanism.
//
// Every test drives the functions directly with made-up facts: no relay, no
// worker, no daemon. That is the point of the module — the decisions these
// tests cover used to live in six places, none of which could be tested
// without a live session.

package core

import (
	"encoding/json"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/schema"
)

const activityMinute uint64 = 60_000
const activityNow int64 = 1_000_000_000

func activityTool(id string, startedAtMs int64) InFlightToolCall {
	return InFlightToolCall{ToolCallID: id, Status: schema.ToolCallStatusInProgress, StartedAtMs: startedAtMs}
}

func activityPolicy(silenceMinutes, toolCallMinutes uint64) StallPolicy {
	var policy StallPolicy
	if silenceMinutes > 0 {
		bound := time.Duration(silenceMinutes*activityMinute) * time.Millisecond
		policy.Silence = &bound
	}
	if toolCallMinutes > 0 {
		bound := time.Duration(toolCallMinutes*activityMinute) * time.Millisecond
		policy.ToolCall = &bound
	}
	return policy
}

// activityFacts is ActivityFacts::default() with the given changes.
func activityFacts(change func(*ActivityFacts)) ActivityFacts {
	facts := DefaultActivityFacts()
	if change != nil {
		change(&facts)
	}
	return facts
}

func agentI64(value int64) *int64 { return &value }
func agentBool(value bool) *bool  { return &value }

func activityQuietNo(reason string) Quiet { return Quiet{Kind: QuietNo, NoReason: reason} }

func activityMinutes(n int64) int64 { return n * int64(activityMinute) }

// The failure in issue #1020: a harness blocked in a long tool call sends no
// protocol traffic for the whole of a twenty-minute build, and used to have
// its turn failed for it.
// Hard-won: 88fbe7a: long-running build/open tool calls were incorrectly reported as a stall.
func TestARunningToolCallIsNotAStall(t *testing.T) {
	facts := activityFacts(func(f *ActivityFacts) {
		f.Execution = RelayExecutionStateRunning
		f.PromptStartedAtMs = agentI64(activityNow - activityMinutes(40))
		f.LastAcpActivityAtMs = agentI64(activityNow - activityMinutes(30))
		f.ToolsInFlight = []InFlightToolCall{activityTool("cargo-nextest", activityNow-activityMinutes(30))}
	})
	if got := StallVerdictOf(&facts, activityPolicy(10, 240), activityNow); got != (StallVerdictLive{}) {
		t.Fatalf("verdict = %#v", got)
	}
}

func TestAToolCallThatOutlivesItsBoundIsAStallNamingTheCall(t *testing.T) {
	facts := activityFacts(func(f *ActivityFacts) {
		f.Execution = RelayExecutionStateRunning
		f.LastAcpActivityAtMs = agentI64(activityNow - activityMinutes(250))
		f.ToolsInFlight = []InFlightToolCall{activityTool("job-output", activityNow-activityMinutes(250))}
	})
	want := StallVerdictToolCall{ToolCallID: "job-output", RunningMs: 250 * activityMinute, SilentMs: 250 * activityMinute}
	if got := StallVerdictOf(&facts, activityPolicy(10, 240), activityNow); got != want {
		t.Fatalf("verdict = %#v, want %#v", got, want)
	}
}

// A turn with no recorded activity at all has not started; absence of
// evidence must not be read as silence.
func TestAnEmptyActivityClockIsNotAStall(t *testing.T) {
	facts := DefaultActivityFacts()
	if got := StallVerdictOf(&facts, activityPolicy(10, 240), activityNow); got != (StallVerdictLive{}) {
		t.Fatalf("verdict = %#v", got)
	}
}

// What is in flight changes while the watchdog waits, so it never sleeps
// through to a bound it worked out from facts that have since moved on. This
// was a real bug: with a long bound the watchdog slept past a tool call
// opening and ending, and the tool-call bound never tripped at all.
// Hard-won: a11622f: watchdog sleep across an open/close transition delayed enforcement of the tool bound.
func TestTheWatchdogNeverSleepsPastAChangeInWhatIsInFlight(t *testing.T) {
	withTool := activityFacts(func(f *ActivityFacts) {
		f.LastAcpActivityAtMs = agentI64(activityNow - activityMinutes(9))
		f.ToolsInFlight = []InFlightToolCall{activityTool("build", activityNow-activityMinutes(100))}
	})
	silent := activityFacts(func(f *ActivityFacts) {
		f.LastAcpActivityAtMs = agentI64(activityNow - activityMinutes(9))
	})
	empty := DefaultActivityFacts()
	for _, facts := range []*ActivityFacts{&withTool, &silent, &empty} {
		wait := activityPolicy(10, 240).NextCheck(facts, activityNow)
		if wait < StallPolicyCheckFloor || wait > StallPolicyCheckCeiling {
			t.Fatalf("%+v waited %v", facts, wait)
		}
	}

	// A bound that is closer than the ceiling is waited on exactly.
	nearlySilent := activityFacts(func(f *ActivityFacts) {
		f.LastAcpActivityAtMs = agentI64(activityNow - 9_800)
	})
	bound := 10 * time.Second
	if got := (StallPolicy{Silence: &bound}).NextCheck(&nearlySilent, activityNow); got != 200*time.Millisecond {
		t.Fatalf("next check = %v", got)
	}
}

// The failure in issue #1025: while the daemon cannot see the worker it used
// to report the default value of an enum, which is `Idle`, about a session
// whose turn was still running.
// Hard-won: 88fbe7a: missing live activity after daemon disconnect was incorrectly treated as idle.
func TestADisconnectedDaemonNeverReportsIdle(t *testing.T) {
	state := WhileDisconnected(MaterializedExecutionStateRunning{StartedAtMs: activityNow - activityMinutes(70)}, agentI64(activityNow))
	if state.IsIdle() || !state.HasWorkInFlight() || !state.IsWorking() {
		t.Fatalf("%+v", state)
	}
	if state.ChatPhase() != RelayExecutionStateRunning {
		t.Fatalf("chat phase = %v", state.ChatPhase())
	}
	want := ActivityState{Kind: ActivityStateTurn, StartedAtMs: agentI64(activityNow - activityMinutes(70))}
	if !reflect.DeepEqual(*state.LastKnownState(), want) {
		t.Fatalf("last known = %+v", *state.LastKnownState())
	}

	wasIdle := WhileDisconnected(MaterializedExecutionStateIdle{}, agentI64(activityNow))
	if wasIdle.IsIdle() {
		t.Fatal("unknown is never confirmed idle")
	}
	if wasIdle.ChatPhase() != RelayExecutionStateIdle {
		t.Fatalf("chat phase = %v", wasIdle.ChatPhase())
	}
	// Even when the last thing known was idleness, the session may have
	// started a turn since. Killing its worker or cutting a checkpoint
	// against it could destroy work, so it still counts as holding some.
	if !wasIdle.HasWorkInFlight() {
		t.Fatal("a session nobody can see is never safe to replace")
	}
	if wasIdle.IsWorking() {
		t.Fatal("but it is not known to be computing")
	}
}

// A worker that is truly gone must still be recoverable: reporting `Unknown`
// about it would make the restart and checkpoint paths wait forever for work
// that no longer exists.
func TestALostWorkerIsStillRecovered(t *testing.T) {
	closed := WhileDisconnected(MaterializedExecutionStateClosed{}, agentI64(activityNow))
	if !reflect.DeepEqual(closed, ActivityState{Kind: ActivityStateClosed}) {
		t.Fatalf("closed = %+v", closed)
	}
	if closed.HasWorkInFlight() {
		t.Fatal("a closed session holds nothing")
	}
	if closed.IsWorking() {
		t.Fatal("closed is not working")
	}

	// The same through the facts the controller syncs from a worker that has
	// stopped: nothing blocks the restart path.
	facts := activityFacts(func(f *ActivityFacts) {
		f.Execution = RelayExecutionStateClosed
		// Stale live evidence from before it stopped must not resurrect it.
		f.ToolsInFlight = []InFlightToolCall{activityTool("orphan", activityNow-activityMinutes(1))}
		f.QueuedCommands = 2
		f.ActiveAgentTerminals = 1
	})
	if !reflect.DeepEqual(Classify(&facts), ActivityState{Kind: ActivityStateClosed}) {
		t.Fatalf("classify = %+v", Classify(&facts))
	}
	if HasWorkInFlight(&facts) {
		t.Fatal("closed facts hold work")
	}
	if reason, blocked := CheckpointBlocker(&facts, HarnessKindKimi); blocked {
		t.Fatalf("checkpoint blocker = %q", reason)
	}
}

// B-5: a Codex adapter that restarted (or a restarted worker) is not
// replaceable until the adapter reports goal and execution state again. The
// blockers say so, and agree with SafeToReplace in every case.
func TestARestartedCodexAdapterBlocksReplacementUntilItReportsAgain(t *testing.T) {
	restarted := activityFacts(func(f *ActivityFacts) { f.GoalSynchronized = false })
	if SafeToReplace(&restarted, HarnessKindCodex) {
		t.Fatal("restarted Codex is replaceable")
	}
	blockers := ReplacementBlockers(&restarted, HarnessKindCodex)
	if len(blockers) != 1 || !strings.Contains(blockers[0], "has not reported goal and execution state") {
		t.Fatalf("blockers = %q", blockers)
	}
	// The same facts do not block a harness that has no goal state.
	if !SafeToReplace(&restarted, HarnessKindClaude) {
		t.Fatal("Claude is blocked")
	}

	reported := restarted
	reported.GoalSynchronized = true
	if !SafeToReplace(&reported, HarnessKindCodex) {
		t.Fatal("reported Codex is not replaceable")
	}
	if blockers := ReplacementBlockers(&reported, HarnessKindCodex); len(blockers) != 0 {
		t.Fatalf("blockers = %q", blockers)
	}
}

func TestReplacementBlockersAgreeWithSafeToReplace(t *testing.T) {
	busyFacts := []ActivityFacts{
		DefaultActivityFacts(),
		activityFacts(func(f *ActivityFacts) {
			f.GoalSynchronized = true
			f.BackgroundWorkKnown = agentBool(true)
		}),
		activityFacts(func(f *ActivityFacts) {
			f.PromptStartedAtMs = agentI64(activityNow)
			f.GoalSynchronized = true
		}),
		activityFacts(func(f *ActivityFacts) { f.QueuedCommands = 2 }),
		activityFacts(func(f *ActivityFacts) {
			f.CheckpointBarrier = true
			f.GoalSynchronized = true
		}),
		activityFacts(func(f *ActivityFacts) {
			f.BackgroundCommands = 1
			f.GoalSynchronized = true
		}),
		activityFacts(func(f *ActivityFacts) {
			f.Execution = RelayExecutionStateRunning
			f.GoalSynchronized = true
		}),
	}
	for _, facts := range busyFacts {
		for _, harness := range []HarnessKind{HarnessKindCodex, HarnessKindKimi, HarnessKindClaude} {
			if (len(ReplacementBlockers(&facts, harness)) == 0) != SafeToReplace(&facts, harness) {
				t.Fatalf("%v %+v", harness, facts)
			}
		}
	}
}

func TestEveryWayOfBeingBusyIsWorkInFlight(t *testing.T) {
	quiet := activityFacts(func(f *ActivityFacts) {
		f.GoalSynchronized = true
		f.BackgroundWorkKnown = agentBool(true)
	})
	if !IsQuiet(&quiet) {
		t.Fatal("quiet is not quiet")
	}
	if state := Classify(&quiet); !state.IsIdle() {
		t.Fatalf("classify = %+v", state)
	}
	if !SafeToReplace(&quiet, HarnessKindCodex) || !SafeToReplace(&quiet, HarnessKindKimi) {
		t.Fatal("quiet is not replaceable")
	}

	type makeBusy struct {
		reason string
		apply  func(*ActivityFacts)
	}
	// Rows where the session is doing something: it holds work *and* its
	// state says so.
	doing := []makeBusy{
		{"a prompt is in flight", func(f *ActivityFacts) { f.PromptStartedAtMs = agentI64(activityNow) }},
		{"the harness started a turn", func(f *ActivityFacts) { f.HarnessTurnStartedAtMs = agentI64(activityNow) }},
		{"a close is in progress", func(f *ActivityFacts) { f.Execution = RelayExecutionStateClosing }},
		{"a tool call is open", func(f *ActivityFacts) { f.ToolsInFlight = []InFlightToolCall{activityTool("bash", activityNow)} }},
		{"a background command is running", func(f *ActivityFacts) { f.BackgroundCommands = 1 }},
		{"a user shell is open", func(f *ActivityFacts) { f.ActiveUserShells = 1 }},
		{"a goal is running", func(f *ActivityFacts) { f.GoalRunning = true }},
	}
	// Rows where the session holds work without doing anything right now. Its
	// state is idle and it is still not safe to kill the worker: these are
	// separate questions and this is where they legitimately differ.
	holding := []makeBusy{
		// A bare running flag is the projection lagging behind a turn that
		// has ended. It is not enough to claim the agent is working, and far
		// too much to ignore when deciding whether to kill the worker.
		{"the execution flag says running", func(f *ActivityFacts) { f.Execution = RelayExecutionStateRunning }},
		{"a command is queued", func(f *ActivityFacts) { f.QueuedCommands = 1 }},
		{"an agent terminal is open", func(f *ActivityFacts) { f.ActiveAgentTerminals = 1 }},
		{"a goal resume is pending", func(f *ActivityFacts) { f.GoalPendingResume = true }},
		{"a goal decision is waiting", func(f *ActivityFacts) { f.GoalDecision = true }},
		{"the ACP session is not open yet", func(f *ActivityFacts) { f.AcpReady = agentBool(false) }},
		{"provider background work is unknown", func(f *ActivityFacts) { f.BackgroundWorkKnown = agentBool(false) }},
	}
	for _, row := range append(append([]makeBusy{}, doing...), holding...) {
		facts := quiet
		row.apply(&facts)
		if !HasWorkInFlight(&facts) || IsQuiet(&facts) || SafeToReplace(&facts, HarnessKindClaude) {
			t.Fatal(row.reason)
		}
	}
	for _, row := range doing {
		facts := quiet
		row.apply(&facts)
		if state := Classify(&facts); state.IsIdle() {
			t.Fatal(row.reason)
		}
	}
	for _, row := range holding {
		facts := quiet
		row.apply(&facts)
		if state := Classify(&facts); !state.IsIdle() {
			t.Fatalf("nothing is running, so the session reads idle even though it still holds work: %s", row.reason)
		}
	}

	// A held barrier is the one reason IsQuiet refuses that is not itself
	// work: a caller already holding one asks whether anything else is.
	latched := quiet
	latched.CheckpointBarrier = true
	if IsQuiet(&latched) || HasWorkInFlight(&latched) {
		t.Fatal("latched barrier")
	}
}

// A running flag needs something live to corroborate it, because the durable
// projection can lag behind a turn that has already ended. With corroboration
// the session is working; without it the flag alone proves nothing.
// Hard-won: 52aba5d: a stale running flag made a restarted worker appear idle and eligible for destructive replacement.
func TestARunningFlagAloneIsNotARunningTurn(t *testing.T) {
	flagOnly := activityFacts(func(f *ActivityFacts) { f.Execution = RelayExecutionStateRunning })
	if state := Classify(&flagOnly); !state.IsIdle() {
		t.Fatalf("classify = %+v", state)
	}
	if !HasWorkInFlight(&flagOnly) {
		t.Fatal("but it is still not safe to kill")
	}
	// The phase a session reports keeps naming the flag, so nothing that read
	// ChatPhase before reads something weaker now.
	if ChatPhase(&flagOnly) != RelayExecutionStateRunning {
		t.Fatal("chat phase")
	}

	for _, corroboration := range []func(*ActivityFacts){
		func(f *ActivityFacts) { f.CurrentStepStartedAtMs = agentI64(activityNow) },
		func(f *ActivityFacts) { f.GoalRunning = true },
		func(f *ActivityFacts) { f.PromptStartedAtMs = agentI64(activityNow) },
	} {
		facts := flagOnly
		corroboration(&facts)
		if state := Classify(&facts); !state.IsWorking() {
			t.Fatalf("%+v", facts)
		}
		if ChatPhase(&facts) != RelayExecutionStateRunning {
			t.Fatalf("%+v", facts)
		}
	}

	// And a live turn the flag has not caught up with still reports running.
	lagging := activityFacts(func(f *ActivityFacts) {
		f.Execution = RelayExecutionStateIdle
		f.HarnessTurnStartedAtMs = agentI64(activityNow)
	})
	if ChatPhase(&lagging) != RelayExecutionStateRunning {
		t.Fatal("lagging chat phase")
	}
}

// A newer worker may publish a state this build does not know. Failing to
// deserialize it would drop the whole snapshot and take every other fact in
// it with it, so an unknown state lands on a cautious value instead.
func TestAnUnrecognizedPublishedStateIsCautiousRatherThanFatal(t *testing.T) {
	var state ActivityState
	if err := json.Unmarshal([]byte(`{"state": "compacting", "started_at_ms": 12345}`), &state); err != nil {
		t.Fatalf("an unknown state must still deserialize: %v", err)
	}
	if !reflect.DeepEqual(state, ActivityState{Kind: ActivityStateUnrecognized}) {
		t.Fatalf("state = %+v", state)
	}
	if state.IsIdle() || !state.HasWorkInFlight() || state.ChatPhase() != RelayExecutionStateRunning {
		t.Fatalf("state = %+v", state)
	}

	// And a state this build does know still round-trips unchanged.
	known := ActivityState{Kind: ActivityStateTool, ToolCallID: "bash", StartedAtMs: agentI64(7), LastActivityAtMs: agentI64(5)}
	text, err := json.Marshal(known)
	if err != nil {
		t.Fatalf("serialize: %v", err)
	}
	var roundTrip ActivityState
	if err := json.Unmarshal(text, &roundTrip); err != nil {
		t.Fatalf("round trip: %v", err)
	}
	if !reflect.DeepEqual(roundTrip, known) {
		t.Fatalf("round trip = %+v", roundTrip)
	}
}

func TestCheckpointBlockerAsksOnlyAboutProviderOwnedWork(t *testing.T) {
	// A turn does not make a held barrier's cut wrong; it is a reason not to
	// start one, which RoutineCheckpointWait answers.
	working := activityFacts(func(f *ActivityFacts) {
		f.Execution = RelayExecutionStateRunning
		f.GoalSynchronized = true
		f.BackgroundWorkKnown = agentBool(true)
	})
	if _, blocked := CheckpointBlocker(&working, HarnessKindClaude); blocked {
		t.Fatal("working Claude is blocked")
	}
	if !HasWorkInFlight(&working) {
		t.Fatal("working holds no work")
	}

	unsynchronizedCodex := activityFacts(func(f *ActivityFacts) { f.GoalSynchronized = false })
	if _, blocked := CheckpointBlocker(&unsynchronizedCodex, HarnessKindCodex); !blocked {
		t.Fatal("unsynchronized Codex is not blocked")
	}
	if _, blocked := CheckpointBlocker(&unsynchronizedCodex, HarnessKindClaude); blocked {
		t.Fatal("Claude is blocked")
	}

	unknownKimi := activityFacts(func(f *ActivityFacts) { f.BackgroundWorkKnown = nil })
	if _, blocked := CheckpointBlocker(&unknownKimi, HarnessKindKimi); !blocked {
		t.Fatal("unknown Kimi is not blocked")
	}

	busyKimi := activityFacts(func(f *ActivityFacts) {
		f.BackgroundWorkKnown = agentBool(true)
		f.BackgroundCommands = 1
	})
	if _, blocked := CheckpointBlocker(&busyKimi, HarnessKindKimi); !blocked {
		t.Fatal("busy Kimi is not blocked")
	}
}

// A routine checkpoint waits for everything that holds work, not only for
// provider-owned work. A Claude session whose agent left a background shell
// running has no provider blocker at all, and it still has to wait: that gap
// is what let the recovery coordinator start a copy every second that the
// barrier then deferred.
// Hard-won: 670a5bb: recovery-copy work looped for hours and filled disk because checkpoint admission ignored provider work.
func TestARoutineCheckpointWaitsForProviderWorkAndForWorkInFlight(t *testing.T) {
	quiet := activityFacts(func(f *ActivityFacts) {
		f.GoalSynchronized = true
		f.BackgroundWorkKnown = agentBool(true)
	})
	for _, harness := range []HarnessKind{HarnessKindClaude, HarnessKindCodex, HarnessKindKimi} {
		if wait := RoutineCheckpointWait(&quiet, harness); wait != nil {
			t.Fatalf("%v: %#v", harness, wait)
		}
	}

	backgroundShell := quiet
	backgroundShell.BackgroundCommands = 1
	if _, blocked := CheckpointBlocker(&backgroundShell, HarnessKindClaude); blocked {
		t.Fatal("background shell has a provider blocker")
	}
	if wait := RoutineCheckpointWait(&backgroundShell, HarnessKindClaude); wait != (CheckpointWaitWorkInFlight{}) {
		t.Fatalf("wait = %#v", wait)
	}

	// Provider-owned work is named, so the deferral can say what it waits for.
	backgroundAgent := quiet
	backgroundAgent.BackgroundCommands = 1
	wait, ok := RoutineCheckpointWait(&backgroundAgent, HarnessKindKimi).(CheckpointWaitProviderWork)
	if !ok || !strings.Contains(wait.Reason, "background agents") {
		t.Fatalf("wait = %#v", wait)
	}

	// A stopped worker holds nothing, so there is nothing to wait for.
	closed := quiet
	closed.Execution = RelayExecutionStateClosed
	closed.BackgroundCommands = 1
	if wait := RoutineCheckpointWait(&closed, HarnessKindKimi); wait != nil {
		t.Fatalf("wait = %#v", wait)
	}
}

// A session nobody can see reports no silence age.
//
// The daemon losing sight of a worker says nothing about whether the harness
// is talking, and presenting a disconnection as harness silence would send a
// reader to cancel a turn that is running perfectly well.
func TestADisconnectedSessionReportsNoSilenceAge(t *testing.T) {
	unknown := WhileDisconnected(MaterializedExecutionStateRunning{StartedAtMs: activityNow - activityMinutes(5)}, agentI64(activityNow))
	if !unknown.IsWorking() {
		t.Fatalf("%+v", unknown)
	}
	if _, ok := unknown.SilentForMs(activityNow); ok {
		t.Fatal("silence age reported")
	}
	if note, ok := SilenceNote(&unknown, activityNow); ok {
		t.Fatalf("silence note = %q", note)
	}
}

func TestInferredIdlePreservesOwnedBackgroundWorkAndForegroundPrecedence(t *testing.T) {
	facts := activityFacts(func(f *ActivityFacts) {
		f.BackgroundCommands = 4
		f.InferredIdleSinceMs = agentI64(100)
	})
	if got := Classify(&facts); !reflect.DeepEqual(got, ActivityState{Kind: ActivityStateIdle, SinceMs: agentI64(100)}) {
		t.Fatalf("classify = %+v", got)
	}
	if !HasWorkInFlight(&facts) || IsQuiet(&facts) || SafeToReplace(&facts, HarnessKindClaude) {
		t.Fatal("inferred idle discharged owned work")
	}
	facts.ActiveUserShells = 1
	if Classify(&facts).Kind != ActivityStateBackground {
		t.Fatal("user shell is not background")
	}
	facts.ActiveUserShells = 0
	facts.PromptStartedAtMs = agentI64(200)
	if Classify(&facts).Kind != ActivityStateTurn {
		t.Fatal("prompt is not a turn")
	}
	facts.PromptStartedAtMs = nil
	facts.GoalRunning = true
	if state := Classify(&facts); state.IsIdle() {
		t.Fatal("running goal is idle")
	}
	facts.GoalRunning = false
	facts.CapacityRetryArmed = true
	if state := Classify(&facts); state.IsIdle() {
		t.Fatal("armed retry is idle")
	}
}

// Launch finding R5-10: "Smoke test passed in 2 second(s)." A duration
// agrees its unit with the count, as every counted noun does since C-12.
// Hard-won: 25c6af8: user-facing durations paired counts with the wrong noun forms, including “2 second(s)”.
func TestADurationAgreesItsUnitWithTheCount(t *testing.T) {
	for _, row := range []struct {
		millis uint64
		want   string
	}{
		{1_000, "1 second"},
		{2_400, "2 seconds"},
		{0, "0 seconds"},
		{90_000, "about 1 minute"},
		{11 * activityMinute, "about 11 minutes"},
	} {
		if got := DescribeDuration(row.millis); got != row.want {
			t.Fatalf("DescribeDuration(%d) = %q, want %q", row.millis, got, row.want)
		}
	}
}

// --- the layered quiet rule -------------------------------------------------

// Hard-won: 295a3f8: upgrade admission killed a Claude notification turn in the task-settle gap.
func TestASettledBackgroundTaskIsAnImminentTurnForTheWindow(t *testing.T) {
	now := int64(5_000_000)
	facts := activityFacts(func(f *ActivityFacts) { f.TaskSettledAtMs = agentI64(now - 1_000) })
	if !TurnImminent(&facts, now) {
		t.Fatal("not imminent")
	}
	if got := QuietAt(&facts, now); got != activityQuietNo("background task settled; its notification turn is about to start") {
		t.Fatalf("quiet = %+v", got)
	}
	if !HasWorkInFlightAt(&facts, now) {
		t.Fatal("no work in flight")
	}
	// The window closes on its own.
	if TurnImminent(&facts, now+ImminentTurnWindowMs+1) {
		t.Fatal("window did not close")
	}
	if got := QuietAt(&facts, now+ImminentTurnWindowMs+1); got != (Quiet{Kind: QuietYes}) {
		t.Fatalf("quiet = %+v", got)
	}
	// Once the turn the settle promised has opened, the turn itself is the
	// reason, and the settle no longer counts on its own.
	turned := facts
	turned.HarnessTurnStartedAtMs = agentI64(now)
	if TurnImminent(&turned, now) {
		t.Fatal("turned is imminent")
	}
	if got := QuietAt(&turned, now); got != activityQuietNo("turn in flight") {
		t.Fatalf("quiet = %+v", got)
	}
	// A settle in the future (clock skew) does not count either.
	skewed := activityFacts(func(f *ActivityFacts) { f.TaskSettledAtMs = agentI64(now + 10_000) })
	if TurnImminent(&skewed, now) {
		t.Fatal("skewed is imminent")
	}
}

func TestJevMayDischargeLeftoverProcessesButNeverATurnOrAShell(t *testing.T) {
	now := int64(5_000_000)
	leftover := activityFacts(func(f *ActivityFacts) {
		f.BackgroundCommands = 1
		f.BackgroundStartedAtMs = agentI64(now - 60_000)
	})
	with := func(base ActivityFacts, change func(*ActivityFacts)) *ActivityFacts {
		change(&base)
		return &base
	}
	if got := QuietAt(&leftover, now); got != activityQuietNo("background work") {
		t.Fatalf("quiet = %+v", got)
	}
	if got := QuietAt(with(leftover, func(f *ActivityFacts) { f.BackgroundNeeded = agentBool(true) }), now); got != activityQuietNo("background work") {
		t.Fatalf("quiet = %+v", got)
	}
	unneeded := *with(leftover, func(f *ActivityFacts) { f.BackgroundNeeded = agentBool(false) })
	if got := QuietAt(&unneeded, now); got != (Quiet{Kind: QuietYes}) {
		t.Fatalf("quiet = %+v", got)
	}
	if HasWorkInFlightAt(&unneeded, now) {
		t.Fatal("unneeded holds work")
	}
	if Classify(&unneeded).Kind != ActivityStateIdle {
		t.Fatal("unneeded is not idle")
	}
	if DriverPresent(&unneeded) {
		t.Fatal("unneeded has a driver")
	}
	// An agent terminal is judged with the commands; a user's shell is not.
	if got := QuietAt(with(unneeded, func(f *ActivityFacts) { f.ActiveAgentTerminals = 1 }), now); got != (Quiet{Kind: QuietYes}) {
		t.Fatalf("quiet = %+v", got)
	}
	if got := QuietAt(with(unneeded, func(f *ActivityFacts) { f.ActiveUserShells = 1 }), now); got != activityQuietNo("user shell open") {
		t.Fatalf("quiet = %+v", got)
	}
	// Observed turn facts outrank the judgment.
	pending := with(unneeded, func(f *ActivityFacts) {
		f.ToolsInFlight = []InFlightToolCall{{ToolCallID: "t", Status: schema.ToolCallStatusPending, StartedAtMs: now - 1_000}}
	})
	if got := QuietAt(pending, now); got != activityQuietNo("tool call in flight") {
		t.Fatalf("quiet = %+v", got)
	}
	if got := QuietAt(with(unneeded, func(f *ActivityFacts) { f.TaskSettledAtMs = agentI64(now - 500) }), now); got != activityQuietNo("background task settled; its notification turn is about to start") {
		t.Fatalf("quiet = %+v", got)
	}
	if got := QuietAt(with(unneeded, func(f *ActivityFacts) { f.QueuedCommands = 1 }), now); got != activityQuietNo("queued command") {
		t.Fatalf("quiet = %+v", got)
	}
	if got := QuietAt(with(unneeded, func(f *ActivityFacts) { f.GoalRunning = true }), now); got != activityQuietNo("turn in flight") {
		t.Fatalf("quiet = %+v", got)
	}
	// A held barrier is not work, but it is not "quiet" either.
	if got := QuietAt(with(unneeded, func(f *ActivityFacts) { f.CheckpointBarrier = true }), now); got != (Quiet{Kind: QuietBarrier}) {
		t.Fatalf("quiet = %+v", got)
	}
	if IsQuiet(with(DefaultActivityFacts(), func(f *ActivityFacts) { f.CheckpointBarrier = true })) {
		t.Fatal("barrier is quiet")
	}
}

// Hard-won: #1498: idle goal intent blocked worker replacement and Move.
func TestDurableGoalIntentAllowsReplacementWithoutDiscardingActualWork(t *testing.T) {
	goal := activityFacts(func(f *ActivityFacts) {
		f.GoalActive = true
		f.GoalSynchronized = true
		f.BackgroundWorkKnown = agentBool(true)
	})
	if state := Classify(&goal); !reflect.DeepEqual(state, ActivityState{Kind: ActivityStateGoal}) || state.HasWorkInFlight() {
		t.Fatalf("classify = %+v", state)
	}
	if !DriverPresent(&goal) || HasWorkInFlight(&goal) || !IsQuiet(&goal) {
		t.Fatal("goal intent")
	}
	for _, harness := range []HarnessKind{HarnessKindClaude, HarnessKindCodex, HarnessKindKimi} {
		if !SafeToReplace(&goal, harness) || !SafeToMove(&goal, harness, MovePurposeManual) {
			t.Fatalf("%v: goal intent blocks replacement", harness)
		}
		if wait := RoutineCheckpointWait(&goal, harness); wait != nil {
			t.Fatalf("%v: wait = %#v", harness, wait)
		}
		if blockers := ReplacementBlockers(&goal, harness); len(blockers) != 0 {
			t.Fatalf("%v: blockers = %q", harness, blockers)
		}
	}
	work := []func(*ActivityFacts){
		func(f *ActivityFacts) { f.GoalRunning = true },
		func(f *ActivityFacts) { f.GoalPendingResume = true },
		func(f *ActivityFacts) { f.GoalDecision = true },
		func(f *ActivityFacts) { f.PromptStartedAtMs = agentI64(activityNow) },
		func(f *ActivityFacts) { f.ToolsInFlight = []InFlightToolCall{activityTool("bash", activityNow)} },
		func(f *ActivityFacts) { f.BackgroundCommands = 1 },
		func(f *ActivityFacts) { f.AcpReady = agentBool(false) },
		func(f *ActivityFacts) { f.BackgroundWorkKnown = agentBool(false) },
		func(f *ActivityFacts) { f.CapacityRetryArmed = true },
	}
	for _, addWork := range work {
		busy := goal
		addWork(&busy)
		if !HasWorkInFlight(&busy) ||
			SafeToReplace(&busy, HarnessKindCodex) ||
			SafeToMove(&busy, HarnessKindCodex, MovePurposeQuotaRetry) ||
			RoutineCheckpointWait(&busy, HarnessKindCodex) == nil {
			t.Fatalf("%+v", busy)
		}
	}
	running := goal
	running.GoalRunning = true
	if state := Classify(&running); state.Kind != ActivityStateTurn || !state.HasWorkInFlight() {
		t.Fatalf("running = %+v", state)
	}
	queued := goal
	queued.QueuedCommands = 1
	if !HasWorkInFlight(&queued) ||
		SafeToReplace(&queued, HarnessKindCodex) ||
		SafeToMove(&queued, HarnessKindCodex, MovePurposeManual) ||
		RoutineCheckpointWait(&queued, HarnessKindCodex) == nil ||
		SafeToMove(&queued, HarnessKindCodex, MovePurposeQuotaRetry) {
		t.Fatal("queued")
	}
	closed := goal
	closed.Execution = RelayExecutionStateClosed
	closed.GoalRunning = true
	if DriverPresent(&closed) {
		t.Fatal("closed has a driver")
	}
	unknown := goal
	unknown.GoalSynchronized = false
	if SafeToReplace(&unknown, HarnessKindCodex) || RoutineCheckpointWait(&unknown, HarnessKindCodex) == nil {
		t.Fatal("unknown")
	}
}

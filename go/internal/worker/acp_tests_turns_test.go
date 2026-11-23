// Port of mj-worker/src/acp/tests.rs: the tests of running turns: the stall
// watchdog, the OpenCode tool-registry hold, steering, and cancellation with
// the bridge restart it can cause.
//
// Rust runs the cancellation and steering-timeout tests on tokio's paused
// clock and advances it past the 60-second cancel and 30-second steering
// bounds. Go has no paused clock for code that waits on real streams, so
// these tests wait in real time and run in parallel with each other.

package worker

import (
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func acpTestsDuration(duration time.Duration) *time.Duration { return &duration }

// Issue #1020: a harness blocked inside one long tool call sends nothing at
// all, and the watchdog used to fail the turn for it. The tool call is the
// sign of life; only its own, much longer bound may end such a turn.
func TestATurnBlockedInALongToolCallIsNotFailed(t *testing.T) {
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	observed := make(chan string, 256)
	go silentAfterPromptBridge(t, bridgeStream, observed, true)
	defer bridgeStream.Close()
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	spec := silentBridgeSpec(core.StallPolicy{
		Silence:  acpTestsDuration(200 * time.Millisecond),
		ToolCall: acpTestsDuration(3_600 * time.Second),
	})
	// The relay records the tool call the bridge announced; the watchdog reads
	// the same handle, which is the whole of the fix.
	tools := spec.ToolsInFlight
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
	defer driver.abort()
	requests <- acpTestsPromptRequest("prompt-1", "build it")
	var methods []string
	waitForBridgePrompt(t, observed, &methods)
	// Record the tool call the bridge announced, exactly as the worker runtime
	// does when it applies the update to the relay.
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		update, ok := acpTestsUpdateValue(t, event)
		return ok && acpTestsGet(update, "sessionUpdate") == "tool_call"
	})
	tools.Open("long-build", core.EpochMillis())

	// Well past the silence bound, and nothing has arrived since the tool call.
	time.Sleep(1_500 * time.Millisecond)
	event, _, timedOut := acpTestsNext(events, driver, 200*time.Millisecond)
	if !timedOut {
		switch event.(type) {
		case core.RuntimeEventPromptFinished, core.RuntimeEventWarning:
			t.Fatalf("a turn blocked in a tool call must not be failed: %#v", event)
		}
	}

	close(requests)
	driver.waitBriefly(5 * time.Second)
}

// The tool call's own bound is the upper limit that still ends a turn, so a
// bridge that dies leaving a tool card open cannot hold the turn forever. It
// could not be forced in a live session — Muse keeps emitting Reminder tool
// cards, which are activity — so it is covered here.
// Hard-won: a11622f: The watchdog slept past short tool calls, so the advertised tool-call timeout never fired.
func TestAToolCallThatOutlivesItsBoundEndsTheTurn(t *testing.T) {
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	observed := make(chan string, 256)
	go silentAfterPromptBridge(t, bridgeStream, observed, true)
	defer bridgeStream.Close()
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	spec := silentBridgeSpec(core.StallPolicy{
		Silence:  acpTestsDuration(3_600 * time.Second),
		ToolCall: acpTestsDuration(400 * time.Millisecond),
	})
	tools := spec.ToolsInFlight
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
	defer driver.abort()
	requests <- acpTestsPromptRequest("prompt-1", "build it")
	var methods []string
	waitForBridgePrompt(t, observed, &methods)
	tools.Open("long-build", core.EpochMillis())

	warning := waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		_, ok := event.(core.RuntimeEventWarning)
		return ok
	}).(core.RuntimeEventWarning)
	if !strings.Contains(warning.Message, "long-build") {
		t.Fatalf("names the tool call: %s", warning.Message)
	}
	if !strings.Contains(warning.Message, "MJ_TURN_TOOL_STALL_TIMEOUT_MS") {
		t.Fatalf("names the knob that raises the limit: %s", warning.Message)
	}

	finished := waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		_, ok := event.(core.RuntimeEventPromptFinished)
		return ok
	}).(core.RuntimeEventPromptFinished)
	if finished.StopReason != turnStalledStopReason {
		t.Fatalf("stop reason %q", finished.StopReason)
	}

	close(requests)
	driver.waitBriefly(5 * time.Second)
}

// The other half: a harness that really has gone quiet, with nothing in
// flight, still loses its turn, and the reason travels with the outcome
// instead of living only in the transcript.
func TestASilentHarnessFailsTheTurnWithAReason(t *testing.T) {
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	observed := make(chan string, 256)
	go silentAfterPromptBridge(t, bridgeStream, observed, false)
	defer bridgeStream.Close()
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	spec := silentBridgeSpec(core.StallPolicy{
		Silence:  acpTestsDuration(200 * time.Millisecond),
		ToolCall: acpTestsDuration(3_600 * time.Second),
	})
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
	defer driver.abort()
	requests <- acpTestsPromptRequest("prompt-1", "say something")
	var methods []string
	waitForBridgePrompt(t, observed, &methods)

	warning := waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		_, ok := event.(core.RuntimeEventWarning)
		return ok
	}).(core.RuntimeEventWarning)
	if !strings.Contains(warning.Message, "no tool call was open") {
		t.Fatalf("the transcript says why the short bound applied: %s", warning.Message)
	}

	finished := waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		_, ok := event.(core.RuntimeEventPromptFinished)
		return ok
	}).(core.RuntimeEventPromptFinished)
	if finished.StopReason != turnStalledStopReason {
		t.Fatalf("stop reason %q", finished.StopReason)
	}
	if finished.Diagnostic == nil {
		t.Fatal("the outcome carries the reason, not only the transcript")
	}
	if finished.Diagnostic.Code == nil || *finished.Diagnostic.Code != turnStalledStopReason {
		t.Fatalf("diagnostic code %v", finished.Diagnostic.Code)
	}
	if !strings.Contains(finished.Diagnostic.Message, "stopped responding") {
		t.Fatalf("diagnostic %#v", finished.Diagnostic)
	}

	close(requests)
	driver.waitBriefly(5 * time.Second)
}

// OpenCode before 2.2.6 reloads its tool registry 100 ms after an MCP server
// connects and signals nobody, so its first prompt waits 250 ms from the open
// (a prompt queued at open is held; a later prompt is not). A version that is
// missing or unparseable takes the same safe side. 2.2.6 and later wait for
// the tools themselves (anomalyco/opencode#60490) and get no hold.
//
// Rust measures on a paused clock, where an unheld prompt takes no time at
// all; in real time an unheld prompt arrives well inside the hold.
func TestTheFirstPromptOfAnOlderOpencodeWaitsForTheToolRegistryAnd226DoesNot(t *testing.T) {
	const hold = 250 * time.Millisecond
	for _, scenario := range []struct {
		version *string
		held    bool
	}{
		{acpTestsStringPointer("2.2.1"), true},
		{acpTestsStringPointer("2.2.5"), true},
		{acpTestsStringPointer("not-a-version"), true},
		{nil, true},
		{acpTestsStringPointer("2.2.6"), false},
		{acpTestsStringPointer("2.3.0"), false},
	} {
		version := "<none>"
		if scenario.version != nil {
			version = *scenario.version
		}
		client, bridgeStream := acpTestsDuplex(64 * 1024)
		observed := make(chan string, 256)
		go promptEchoingBridgeReporting(t, bridgeStream, observed, scenario.version)
		spec := reloadFallbackSpec(core.HarnessKindOpenCode)
		spec.ResumeSession = nil
		requests := make(chan CommandRequest, 4)
		events := make(chan core.RuntimeEvent, 64)
		requests <- acpTestsPromptRequest("first", "first")
		driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
		observe := func(expected string) time.Time {
			t.Helper()
			for {
				if acpTestsReceive(t, observed, ioTimeout, "the bridge keeps reporting") == expected {
					return time.Now()
				}
			}
		}
		opened := observe("session/new")
		first := observe("session/prompt:first")
		if waited := first.Sub(opened); scenario.held && waited < hold {
			t.Fatalf("the first prompt reached OpenCode %s %v after the open", version, waited)
		} else if !scenario.held && waited >= hold {
			t.Fatalf("OpenCode %s must not hold its first prompt: it waited %v", version, waited)
		}
		waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
			finished, ok := event.(core.RuntimeEventPromptFinished)
			return ok && finished.RequestID == "first"
		})
		sent := time.Now()
		requests <- acpTestsPromptRequest("second", "second")
		if waited := observe("session/prompt:second").Sub(sent); waited >= hold {
			t.Fatalf("a later prompt must not be delayed: it waited %v", waited)
		}
		close(requests)
		if _, err := driver.wait(t, 5*time.Second, "the runtime ends with its requests"); err != nil {
			t.Fatal(err)
		}
		bridgeStream.Close()
	}
}

func TestCancelSteersTheQueuedPromptWhenTheAgentSupportsIt(t *testing.T) {
	exerciseImageSteering(t, false, "injected")
}

func TestTenLargePhotosReachAcpForBothPromptAndSteering(t *testing.T) {
	started := time.Now()
	exerciseImageSteering(t, true, "injected")
	if elapsed := time.Since(started); elapsed > 20*time.Second {
		t.Fatalf("large image delivery must not deadlock: it took %v", elapsed)
	}
}

func TestSteeringTimeoutKeepsObservingAndRepeatedEscapeNeverCancels(t *testing.T) {
	t.Parallel()
	exerciseImageSteering(t, false, "hold")
}

func TestFailedSteeringDoesNotFallBackToCancellation(t *testing.T) {
	exerciseImageSteering(t, false, "failed")
}

func TestUnexpectedSteeringDeliveryIsHeldWithoutCancellation(t *testing.T) {
	exerciseImageSteering(t, false, "startedNewTurn")
}

func TestReturnedSteeringLeavesThePromptToMjWithoutCancellation(t *testing.T) {
	exerciseImageSteering(t, false, "promptRequired")
}

// Hard-won: 410851f: A Kimi bridge restarted unnecessarily after it acknowledged cancellation.
func TestAcknowledgedCancelKeepsTheBridgeForTheNextPrompt(t *testing.T) {
	t.Parallel()
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	observed := make(chan string, 256)
	complete := make(chan struct{}, 1)
	go stalledPromptBridge(t, bridgeStream, observed, complete)
	defer bridgeStream.Close()
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	spec := acpTestsSpec(core.HarnessKindKimi, "scripted", acpTestsCurrentDir(t))
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
	defer driver.abort()
	var methods []string
	requests <- acpTestsPromptRequest("prompt-1", "go")
	waitForBridgePrompt(t, observed, &methods)
	requests <- CommandRequestCancel{RequestID: "cancel-1"}
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		applied, ok := event.(core.RuntimeEventCancelApplied)
		return ok && applied.RequestID == "cancel-1"
	})
	// Rust advances its paused clock to one second before the bound. A real
	// clock keeps a wider margin, so scheduling cannot carry the
	// acknowledgement past it.
	time.Sleep(cancelAckTimeout - 5*time.Second)
	complete <- struct{}{}
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		finished, ok := event.(core.RuntimeEventPromptFinished)
		return ok && finished.RequestID == "prompt-1"
	})

	// The acknowledged cancel leaves the bridge in place, so the next prompt
	// runs on the same connection and the same native session.
	requests <- acpTestsPromptRequest("prompt-2", "carry on")
	waitForBridgePrompt(t, observed, &methods)
	complete <- struct{}{}
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		finished, ok := event.(core.RuntimeEventPromptFinished)
		return ok && finished.RequestID == "prompt-2"
	})
	close(requests)
	restart, err := driver.wait(t, 5*time.Second, "runtime exits")
	if err != nil {
		t.Fatalf("a cancelled prompt must not fail the runtime: %v", err)
	}
	// `runInner` emits `HarnessRestarting` exactly when the bridge asks for a
	// restart, so no restart request means no restart event.
	if restart != nil {
		t.Fatalf("an acknowledged cancel must not restart the bridge: %#v", restart)
	}
	for {
		event, ok := acpTestsTryRecv(events)
		if !ok {
			break
		}
		if _, restarting := event.(core.RuntimeEventHarnessRestarting); restarting {
			t.Fatalf("unexpected restart: %#v", event)
		}
	}
	initializes, opens := 0, 0
	for _, method := range methods {
		switch method {
		case "initialize":
			initializes++
		case "session/new", "session/load":
			opens++
		}
	}
	if initializes != 1 {
		t.Fatalf("the second prompt must not re-handshake: %q", methods)
	}
	if opens != 1 {
		t.Fatalf("the second prompt must reuse the open session: %q", methods)
	}
}

func TestUnackedCancelRestartsTheHarnessAfterSixtySeconds(t *testing.T) {
	t.Parallel()
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	observed := make(chan string, 256)
	// Held, never sent and never closed: the prompt is never settled.
	complete := make(chan struct{}, 1)
	go stalledPromptBridge(t, bridgeStream, observed, complete)
	defer bridgeStream.Close()
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	spec := acpTestsSpec(core.HarnessKindKimi, "scripted", acpTestsCurrentDir(t))
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
	defer driver.abort()
	requests <- acpTestsPromptRequest("prompt-1", "go")
	for acpTestsReceive(t, observed, 5*time.Second, "the prompt must reach the bridge") != "session/prompt" {
	}
	requests <- CommandRequestCancel{RequestID: "cancel-1"}
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		applied, ok := event.(core.RuntimeEventCancelApplied)
		return ok && applied.RequestID == "cancel-1"
	})
	interrupted := acpTestsWaitForEventWithin(t, events, cancelAckTimeout+ioTimeout, func(event core.RuntimeEvent) bool {
		interrupted, ok := event.(core.RuntimeEventCommandInterrupted)
		return ok && interrupted.RequestID == "prompt-1"
	}).(core.RuntimeEventCommandInterrupted)
	if !strings.Contains(interrupted.Message, "60s") {
		t.Fatal(interrupted.Message)
	}
	close(requests)
	restart, err := driver.wait(t, 5*time.Second, "runtime exits after an unacked cancel")
	if err != nil {
		t.Fatalf("an unacked cancel restarts instead of failing the runtime: %v", err)
	}
	if resume, ok := restart.(SessionRestartResume); !ok || resume.Value != "scripted" {
		t.Fatalf("restart %#v, want a resume of scripted", restart)
	}
}

// Hard-won: 410851f: A queued Kimi prompt reached a replacement bridge untracked and desynchronized the relay.
func TestARequestQueuedAcrossARestartNeverReachesTheFreshBridge(t *testing.T) {
	t.Parallel()
	scriptedSpec := func(resumeSession *string) LaunchSpec {
		spec := acpTestsSpec(core.HarnessKindKimi, "scripted", acpTestsCurrentDir(t))
		spec.ResumeSession = resumeSession
		return spec
	}

	// The first bridge stalls its prompt, so an unacknowledged cancel restarts
	// the harness the way `runInner` would.
	firstClient, firstBridge := acpTestsDuplex(64 * 1024)
	firstObserved := make(chan string, 256)
	complete := make(chan struct{}, 1)
	go stalledPromptBridge(t, firstBridge, firstObserved, complete)
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	firstDriver := acpTestsStartDrive(acpTestsTransport(firstClient), scriptedSpec(nil), requests, events, NewShared[*OpenedSession](nil), false)
	defer firstDriver.abort()
	requests <- acpTestsPromptRequest("prompt-1", "go")
	var firstMethods []string
	waitForBridgePrompt(t, firstObserved, &firstMethods)
	requests <- CommandRequestCancel{RequestID: "cancel-1"}
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		applied, ok := event.(core.RuntimeEventCancelApplied)
		return ok && applied.RequestID == "cancel-1"
	})
	acpTestsWaitForEventWithin(t, events, cancelAckTimeout+ioTimeout, func(event core.RuntimeEvent) bool {
		interrupted, ok := event.(core.RuntimeEventCommandInterrupted)
		return ok && interrupted.RequestID == "prompt-1"
	})
	restart, err := firstDriver.wait(t, 5*time.Second, "runtime exits after an unacked cancel")
	if err != nil {
		t.Fatalf("an unacked cancel restarts instead of failing the runtime: %v", err)
	}
	if resume, ok := restart.(SessionRestartResume); !ok || resume.Value != "scripted" {
		t.Fatalf("restart %#v, want a resume of scripted", restart)
	}
	firstBridge.Close()

	// The worker queued this before it saw `HarnessRestarting`, so it is
	// already in the set the worker interrupted. The fresh bridge must drop it
	// instead of running it untracked on the reloaded session.
	requests <- acpTestsPromptRequest("late-prompt", "stale")

	secondClient, secondBridge := acpTestsDuplex(64 * 1024)
	secondObserved := make(chan string, 256)
	go promptEchoingBridge(t, secondBridge, secondObserved)
	defer secondBridge.Close()
	secondDriver := acpTestsStartDrive(acpTestsTransport(secondClient), scriptedSpec(acpTestsStringPointer("scripted")), requests, events, NewShared[*OpenedSession](nil), true)
	defer secondDriver.abort()
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		_, ok := event.(core.RuntimeEventSessionConfigured)
		return ok
	})

	// A command dispatched after the worker sees the fresh session does run.
	requests <- acpTestsPromptRequest("after-restart", "fresh")
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		finished, ok := event.(core.RuntimeEventPromptFinished)
		return ok && finished.RequestID == "after-restart"
	})
	close(requests)
	restart, err = secondDriver.wait(t, 5*time.Second, "the second bridge exits when its requests end")
	if err != nil {
		t.Fatalf("the second bridge must not fail the runtime: %v", err)
	}
	if restart != nil {
		t.Fatalf("restart %#v", restart)
	}
	var secondMethods []string
	for {
		method, ok := acpTestsTryRecv(secondObserved)
		if !ok {
			break
		}
		secondMethods = append(secondMethods, method)
	}
	if want := []string{"initialize", "session/load", "session/prompt:fresh"}; !slices.Equal(secondMethods, want) {
		t.Fatalf("the queued prompt must never reach the fresh bridge: %q", secondMethods)
	}
}

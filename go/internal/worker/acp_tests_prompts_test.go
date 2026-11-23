// Port of mj-worker/src/acp/tests.rs: the tests of prompts, their failures,
// forms and execution policies against fake bridges over in-memory streams.

package worker

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Hard-won: a02d1c6: child task updates must not become warning rows in the parent transcript.
func TestAirTasksFromRootAndChildSessionsStopThroughTheOwningAdapter(t *testing.T) {
	for _, harness := range []core.HarnessKind{core.HarnessKindCodex, core.HarnessKindClaude} {
		client, bridgeStream := acpTestsDuplex(64 * 1024)
		go func() {
			bridge := acpTestsBridge(bridgeStream)
			for {
				request, ok := bridge.next(t)
				if !ok {
					return
				}
				method, ok := request["method"].(string)
				if !ok {
					continue
				}
				type ownedUpdate struct {
					owner  string
					update map[string]any
				}
				var updates []ownedUpdate
				var result any
				switch method {
				case "initialize":
					result = map[string]any{"protocolVersion": 1}
				case "session/new":
					updates = append(updates, ownedUpdate{"scripted", map[string]any{"sessionUpdate": "subagent_spawned", "subagentSessionId": "child", "name": "review", "task": "review source", "capabilities": map[string]any{}}})
					for _, owner := range []string{"scripted", "child"} {
						id := "task"
						if owner == "child" && harness == core.HarnessKindCodex {
							id = "child:task"
						}
						spawn := map[string]any{"sessionUpdate": "async_task_spawned", "asyncTaskId": id, "name": "sleep 600", "taskType": "shell", "showInTranscript": false, "canStop": true}
						if harness == core.HarnessKindClaude {
							spawn["description"] = "Background sleeper"
						}
						updates = append(updates,
							ownedUpdate{owner, spawn},
							ownedUpdate{owner, map[string]any{"sessionUpdate": "async_task_progress", "asyncTaskId": id, "description": owner + " sleeper"}},
							ownedUpdate{owner, map[string]any{"sessionUpdate": "async_task_state_update", "asyncTaskId": id, "state": "paused"}})
					}
					updates = append(updates,
						ownedUpdate{"scripted", map[string]any{"sessionUpdate": "async_task_spawned", "asyncTaskId": "turn", "name": "autonomous turn", "taskType": "turn", "canStop": true}},
						ownedUpdate{"scripted", map[string]any{"sessionUpdate": "async_task_state_update", "asyncTaskId": "turn", "state": "completed"}})
					result = map[string]any{"sessionId": "scripted", "modes": map[string]any{"currentModeId": "default", "availableModes": []any{
						map[string]any{"id": "default", "name": "Default"},
						map[string]any{"id": "auto", "name": "Auto"},
						map[string]any{"id": "agent", "name": "Agent"},
					}}}
				case "session/set_mode":
					result = map[string]any{}
				case "_session/async_task/stop":
					owner, _ := acpTestsString(acpTestsGet(request, "params", "sessionId"))
					task, _ := acpTestsString(acpTestsGet(request, "params", "asyncTaskId"))
					if owner != "scripted" && owner != "child" {
						t.Errorf("a stop for an unknown session %q", owner)
						return
					}
					want := "task"
					if owner == "child" && harness == core.HarnessKindCodex {
						want = "child:task"
					}
					if task != want {
						t.Errorf("%s stopped task %q, want %q", owner, task, want)
						return
					}
					if !bridge.send(acpTestsUpdate(owner, map[string]any{"sessionUpdate": "async_task_state_update", "asyncTaskId": task, "state": "stopped"})) {
						return
					}
					result = map[string]any{"stopped": true}
				default:
					continue
				}
				for _, update := range updates {
					if !bridge.send(acpTestsUpdate(update.owner, update.update)) {
						return
					}
				}
				if !bridge.send(acpTestsResult(request["id"], result)) {
					return
				}
			}
		}()
		requests := make(chan CommandRequest, 4)
		events := make(chan core.RuntimeEvent, 32)
		spec := acpTestsSpec(harness, "scripted", acpTestsCurrentDir(t))
		observedStepClock := spec.StepClock
		driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
		relay, err := DurableRelayOpen(t.TempDir(), "air-test", "test")
		if err != nil {
			t.Fatal(err)
		}
		relay.SetBackgroundWorkPolicy(BackgroundWorkPolicyAirTasks)
		if harness == core.HarnessKindClaude {
			relay.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
		} else {
			relay.SetHarnessTurnPolicy(HarnessTurnPolicyCodexAdapter)
		}
		paused := 0
		for paused < 2 {
			switch event := acpTestsExpectEvent(t, events, driver, 5*time.Second, "the AIR tasks arrive", "the runtime stopped").(type) {
			case core.RuntimeEventAirTaskChanged:
				if spawned, ok := event.Update.(core.AirTaskUpdateSpawned); ok {
					want := "sleep 600"
					if harness == core.HarnessKindClaude {
						want = "Background sleeper"
					}
					if spawned.Description != want {
						t.Fatalf("%v: spawned %q, want %q", harness, spawned.Description, want)
					}
				}
				if changed, ok := event.Update.(core.AirTaskUpdateChanged); ok && changed.Description == nil {
					paused++
				}
				if err := relay.AirTaskChanged(event.SessionID, event.Update); err != nil {
					t.Fatal(err)
				}
			case core.RuntimeEventWarning:
				if strings.Contains(event.Message, "async_task") {
					t.Fatal(event.Message)
				}
			}
		}
		if started := observedStepClock.StartedAtMs(); started != nil {
			t.Fatalf("background task updates started a step at %d", *started)
		}
		commands := relay.OperationalState().BackgroundCommands
		if len(commands) != 2 {
			t.Fatalf("%v: background commands %#v", harness, commands)
		}
		for _, command := range commands {
			if !command.CanStop || !strings.HasSuffix(command.Command, " sleeper") {
				t.Fatalf("background command %#v", command)
			}
			target, err := relay.BackgroundTaskStopTarget(command.ID)
			if err != nil {
				t.Fatal(err)
			}
			resolution := make(chan error, 1)
			requests <- CommandRequestStopBackgroundTask{Target: target, Resolved: resolution}
			if err := acpTestsReceive(t, resolution, 5*time.Second, "the stop resolves"); err != nil {
				t.Fatalf("stop %s: %v", command.ID, err)
			}
			for stopped := false; !stopped; {
				switch event := acpTestsExpectEvent(t, events, driver, 5*time.Second, "the stopped task finishes", "the runtime stopped").(type) {
				case core.RuntimeEventAirTaskChanged:
					_, finished := event.Update.(core.AirTaskUpdateFinished)
					if err := relay.AirTaskChanged(event.SessionID, event.Update); err != nil {
						t.Fatal(err)
					}
					if finished && !slices.ContainsFunc(relay.OperationalState().BackgroundCommands, func(entry core.BackgroundCommand) bool {
						return entry.ID == command.ID
					}) {
						stopped = true
					}
				case core.RuntimeEventWarning:
					if strings.Contains(event.Message, "async_task") {
						t.Fatal(event.Message)
					}
				}
			}
		}
		if commands := relay.OperationalState().BackgroundCommands; len(commands) != 0 {
			t.Fatalf("background commands remain: %#v", commands)
		}
		if settled := relay.ActivityFacts().TaskSettledAtMs; settled != nil {
			t.Fatalf("a stopped task is not a settled task: %d", *settled)
		}
		close(requests)
		if _, err := driver.wait(t, 5*time.Second, "the runtime ends with its requests"); err != nil {
			t.Fatal(err)
		}
		bridgeStream.Close()
	}
}

func TestRuntimeEventDeliveryWaitsForBoundedChannelCapacity(t *testing.T) {
	events := make(chan core.RuntimeEvent, 1)
	if err := emitRuntimeEvent(context.Background(), events, core.RuntimeEventWarning{Message: "first"}); err != nil {
		t.Fatal(err)
	}

	blocked := make(chan error, 1)
	go func() {
		blocked <- emitRuntimeEvent(context.Background(), events, core.RuntimeEventWarning{Message: "second"})
	}()
	time.Sleep(20 * time.Millisecond)
	if _, finished := acpTestsTryRecv(blocked); finished {
		t.Fatal("event producer bypassed bounded-channel backpressure")
	}

	if event := <-events; event != (core.RuntimeEventWarning{Message: "first"}) {
		t.Fatalf("first event %#v", event)
	}
	if err := acpTestsReceive(t, blocked, ioTimeout, "the blocked producer delivers"); err != nil {
		t.Fatal(err)
	}
	if event := <-events; event != (core.RuntimeEventWarning{Message: "second"}) {
		t.Fatalf("second event %#v", event)
	}
}

// Launch finding J-25: a Codex quota error showed in the system row as
// `warning: prompt failed: Internal error: { "message": ..., "codexErrorInfo":
// "usageLimitExceeded" }`. The warning is the provider's sentence on one
// line, and the turn ends as a quota stop, as Kimi's limit already does.
// Hard-won: cd2f1b6: Codex quota limits surfaced as raw JSON and repeated the same warning.
func TestACodexUsageLimitEndsAsAQuotaStopWithAReadableWarning(t *testing.T) {
	limit := acpTestsRPCError(rpcCodeInternalError, "Internal error", map[string]any{
		"message":        "You’ve hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits\nor try again at Sep 29th, 2026 10:20 PM.",
		"codexErrorInfo": "usageLimitExceeded",
	})
	diagnostic := core.TurnDiagnosticFromACP(limit)
	stopReason, warning := promptErrorOutcome(core.HarnessKindCodex, limit, &diagnostic, "")
	if stopReason != core.QuotaStopReason {
		t.Fatalf("stop reason %q", stopReason)
	}
	if want := "prompt failed: You’ve hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Sep 29th, 2026 10:20 PM."; warning == nil || *warning != want {
		t.Fatalf("warning %v, want %q", warning, want)
	}

	// Another Codex error names its kind when it has no sentence of its own.
	bare := acpTestsRPCError(rpcCodeInternalError, "Internal error", map[string]any{"codexErrorInfo": "responseStreamDisconnected"})
	bareDiagnostic := core.TurnDiagnosticFromACP(bare)
	stopReason, warning = promptErrorOutcome(core.HarnessKindCodex, bare, &bareDiagnostic, "")
	if stopReason != promptErrorStopReason {
		t.Fatalf("stop reason %q", stopReason)
	}
	if want := "prompt failed: Internal error (responseStreamDisconnected)"; warning == nil || *warning != want {
		t.Fatalf("warning %v, want %q", warning, want)
	}

	// An error whose data carries an authentication code keeps its text:
	// credential sync reads that code from the warning.
	auth := acpTestsRPCError(rpcCodeInternalError, "Internal error", map[string]any{
		"message": "Request failed", "type": "authentication_error",
	})
	authDiagnostic := core.TurnDiagnosticFromACP(auth)
	_, warning = promptErrorOutcome(core.HarnessKindClaude, auth, &authDiagnostic, "Request failed")
	if warning == nil {
		t.Fatal("a warning that carries an auth code is never left out")
	}
	if !core.AuthFailureSignature(core.HarnessKindClaude, *warning) {
		t.Fatal(*warning)
	}
}

func TestAnAuthRequiredPromptFailureCarriesTheCredentialMarker(t *testing.T) {
	auth := promptFailureWarning(core.HarnessKindClaude, acpTestsRPCError(-32000, "Authentication required", nil))
	if !strings.Contains(auth, "prompt failed") {
		t.Fatal(auth)
	}
	if !core.AuthFailureSignature(core.HarnessKindClaude, auth) {
		t.Fatal(auth)
	}

	other := promptFailureWarning(core.HarnessKindClaude, acpTestsRPCError(rpcCodeInternalError, "Internal error", nil))
	if !strings.Contains(other, "prompt failed") {
		t.Fatal(other)
	}
	if core.AuthFailureSignature(core.HarnessKindClaude, other) {
		t.Fatal(other)
	}
}

// R4-7: Kimi fails a prompt past its weekly limit with ACP `auth_required`
// and a 403 usage-limit message (`cli/1136-11-prompt.txt`). The turn ended
// as QuotaLimit, but the conversation said "prompt failed (ACP
// auth_required)", which also reads as a sign-in failure.
// Hard-won: 88ef8fc: Kimi weekly quota errors were labeled authentication failures and suggested re-login.
func TestAUsageLimitSentAsAuthRequiredIsLabelledAUsageLimit(t *testing.T) {
	limit := acpTestsRPCError(-32000, "Authentication required: 403 You've reached your weekly (7-day) usage "+
		"limit. Your quota will reset when the current 7-day window ends. To continue now, "+
		"purchase extra usage or upgrade your plan: "+
		"https://www.kimi.com/membership/subscription?tab=quota", nil)
	label := promptFailureWarning(core.HarnessKindKimi, limit)
	if !strings.HasPrefix(label, "prompt failed (usage limit reached): Authentication required: 403") {
		t.Fatal(label)
	}
	if core.AuthFailureSignature(core.HarnessKindKimi, label) {
		t.Fatalf("a usage limit is not a sign-in failure: %s", label)
	}
}

// Hard-won: #1265: an async question must remain answerable after its originating turn ends.
func TestFormElicitationIsAdvertisedRenderedAndAnswered(t *testing.T) {
	answerArchitectureForm(t, core.HarnessKindClaude, false)
	answerArchitectureForm(t, core.HarnessKindCodex, true)
}

func TestAnUnknownClientRequestIsAnsweredWithAnErrorRatherThanSilence(t *testing.T) {
	answer := answerToExtRequest(t, "_someone.example/unknown", core.ExecutionPolicyUnconstrained)
	if acpTestsHas(answer, "result") {
		t.Fatalf("an unimplemented request must not be answered with a result: %v", answer)
	}
	if code := acpTestsGet(answer, "error", "code"); code != float64(-32601) {
		t.Fatalf("expected a method-not-found error: %v", answer)
	}
}

func TestCodexSelectsGuardianForANewSession(t *testing.T) {
	policyIsEnforcedBeforeSessionIsReported(t, core.HarnessKindCodex, core.ExecutionPolicyConfiguredApprovals,
		core.DefaultApprovalChoices().ClaudePermissionMode, nil)
}

func TestCodexSelectsTargetPolicyWhenLoadingASession(t *testing.T) {
	policyIsEnforcedBeforeSessionIsReported(t, core.HarnessKindCodex, core.ExecutionPolicyConfiguredApprovals,
		core.DefaultApprovalChoices().ClaudePermissionMode, acpTestsStringPointer("native-session"))
}

func TestUnconstrainedPolicyIsEnforcedBeforeTheSessionIsReported(t *testing.T) {
	policyIsEnforcedBeforeSessionIsReported(t, core.HarnessKindCodex, core.ExecutionPolicyUnconstrained,
		core.DefaultApprovalChoices().ClaudePermissionMode, nil)
}

// Claude's guardian policy is the profile's permission mode (Auto unless the
// profile picks another), which it must select before the session is
// reported; unconstrained sessions take bypassPermissions whatever the
// profile picked.
func TestClaudeSelectsTheProfileModeForGuardianAndBypassWhenUnconstrained(t *testing.T) {
	for _, policy := range []core.ExecutionPolicy{core.ExecutionPolicyConfiguredApprovals, core.ExecutionPolicyUnconstrained} {
		for _, mode := range []core.ClaudePermissionMode{
			core.ClaudePermissionModeAuto,
			core.ClaudePermissionModeAcceptEdits,
			core.ClaudePermissionModeManual,
		} {
			policyIsEnforcedBeforeSessionIsReported(t, core.HarnessKindClaude, policy, mode, nil)
		}
	}
}

// Hard-won: 26a181b: a Guardian Muse session started with approvals off when promptUnmatched was not offered
func TestAGuardianMuseWhoseBridgeOffersOnlyAllowAllFailsTheSession(t *testing.T) {
	guardianMuseOfferingOnlyAllowAllFails(t, false)
}

// Muse keeps Plan on its `mode` selector, apart from `approval_mode`, so a
// context restored in Plan must not stand in for the approval policy.
// Hard-won: 46cea84: a Guardian Muse session restored in Plan started with approvals off
func TestAGuardianMuseRestoredInPlanStillNeedsItsApprovalMode(t *testing.T) {
	guardianMuseOfferingOnlyAllowAllFails(t, true)
}

// Hard-won: eed6c53: A harness could acknowledge a permission mode but run under a different, unsafe mode.
func TestAModeTheHarnessAcknowledgesButDoesNotApplyFailsTheSession(t *testing.T) {
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	go stubbornModeBridge(t, bridgeStream)
	defer bridgeStream.Close()
	requests := make(chan CommandRequest, 1)
	events := make(chan core.RuntimeEvent, 16)
	spec := acpTestsSpec(core.HarnessKindClaude, "scripted", acpTestsCurrentDir(t))
	spec.ExecutionPolicy = core.ExecutionPolicyUnconstrained
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)

	_, err := driver.wait(t, 5*time.Second, "the session must stop instead of waiting for prompts")
	if err == nil {
		t.Fatal("an unapplied execution mode must fail the session")
	}
	message := err.Error()
	if !strings.Contains(message, "acknowledged execution mode bypassPermissions") || !strings.Contains(message, "default") {
		t.Fatalf("the failure must name the requested and the reported mode: %s", message)
	}
	for {
		event, ok := acpTestsTryRecv(events)
		if !ok {
			break
		}
		if _, started := event.(core.RuntimeEventSessionStarted); started {
			t.Fatal("no session may be reported once the mode was refused")
		}
	}
	close(requests)
}

// R14-1: Codex could not refresh its login, streamed its sentence as agent
// text and then failed the prompt with the same sentence. The conversation
// showed it twice, as the agent's message and as Mjolnir's warning row, and
// nothing recognized it as an auth failure. The warning now appears only
// when the turn's last agent message does not already say it, it stays one
// readable line that credential sync recognizes, and the failed turn carries
// Codex's error kind either way.
// Hard-won: 1fb50fe: Codex refresh failures skipped credential reconciliation and duplicated their diagnostic.
func TestAFailureTheHarnessAlreadyStreamedIsNotRepeatedAsAWarning(t *testing.T) {
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	go codexRefreshFailureBridge(t, bridgeStream)
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	spec := reloadFallbackSpec(core.HarnessKindCodex)
	spec.ResumeSession = nil
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
	defer func() {
		driver.abort()
		bridgeStream.Close()
	}()

	turn := func(requestID string) []string {
		t.Helper()
		requests <- acpTestsPromptRequest(requestID, "Reply with pong.")
		warnings := []string{}
		for {
			switch event := acpTestsExpectEvent(t, events, driver, 5*time.Second, "the runtime keeps reporting", "the runtime keeps its event channel").(type) {
			case core.RuntimeEventWarning:
				// The session's opening warnings are not about the turn.
				if strings.HasPrefix(event.Message, "prompt failed") {
					warnings = append(warnings, event.Message)
				}
			case core.RuntimeEventPromptFinished:
				if event.StopReason != promptErrorStopReason {
					t.Fatalf("stop reason %q", event.StopReason)
				}
				if event.Diagnostic == nil {
					t.Fatal("a failed prompt carries its diagnostic")
				}
				if event.Diagnostic.Code == nil || *event.Diagnostic.Code != "unauthorized" {
					t.Fatalf("diagnostic code %v", event.Diagnostic.Code)
				}
				if event.Diagnostic.Message != codexRefreshFailure {
					t.Fatalf("diagnostic message %q", event.Diagnostic.Message)
				}
				return warnings
			}
		}
	}

	// Streamed as the last agent message: no second copy.
	if warnings := turn("streamed"); len(warnings) != 0 {
		t.Fatalf("warnings %q", warnings)
	}

	// Not streamed: the warning is the sentence on one line, and credential
	// sync recognizes it.
	expected := "prompt failed: " + codexRefreshFailure
	warnings := turn("silent")
	if !slices.Equal(warnings, []string{expected}) {
		t.Fatalf("warnings %q", warnings)
	}
	if !core.AuthFailureSignature(core.HarnessKindCodex, warnings[0]) {
		t.Fatal(warnings[0])
	}

	// A thought ended the message that said it, so the warning is not a
	// repeat of the last agent message.
	if warnings := turn("thought"); !slices.Equal(warnings, []string{expected}) {
		t.Fatalf("warnings %q", warnings)
	}
}

// Hard-won: #1515: delivery before the auth failure lost the rejected login's identity.
func TestAFailedPromptFailsTheTurnAndTheRuntimeKeepsServing(t *testing.T) {
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	home := t.TempDir()
	loginPath := filepath.Join(home, ".credentials.json")
	if err := os.WriteFile(loginPath, []byte(`{"claudeAiOauth":{"accessToken":"old-access","expiresAt":1000}}`), 0o600); err != nil {
		t.Fatal(err)
	}
	loginIdentity := &ClaudeLoginIdentity{}
	oldFingerprint := core.CredentialFingerprint([]byte("old-access"))
	loginIdentity.installed(&oldFingerprint)
	prompts := make(chan int, 1)
	go func() { prompts <- scriptedBridge(t, bridgeStream, loginPath, loginIdentity) }()

	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 16)
	spec := acpTestsSpec(core.HarnessKindClaude, "scripted", acpTestsCurrentDir(t))
	spec.LoginIdentity = loginIdentity
	spec.Environment = map[string]string{"CLAUDE_CONFIG_DIR": home}
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)

	nextEvent := func() core.RuntimeEvent {
		t.Helper()
		return acpTestsExpectEvent(t, events, driver, 5*time.Second,
			"the runtime must keep emitting events after a failed prompt", "the runtime must not drop its event channel")
	}

	requests <- acpTestsPromptRequest("first", "hello")
	var warning *string
	var failed core.RuntimeEventPromptFinished
	for done := false; !done; {
		switch event := nextEvent().(type) {
		case core.RuntimeEventWarning:
			warning = &event.Message
		case core.RuntimeEventPromptFinished:
			failed, done = event, true
		}
	}
	if failed.RequestID != "first" || failed.StopReason != "error" {
		t.Fatalf("finished %q with %q", failed.RequestID, failed.StopReason)
	}
	if failed.Diagnostic == nil {
		t.Fatal("auth failure retains its diagnostic")
	}
	if !core.TurnDiagnosticReportsAuthFailure(failed.Diagnostic) {
		t.Fatalf("diagnostic %#v", failed.Diagnostic)
	}
	if failed.Diagnostic.LoginFingerprint == nil || *failed.Diagnostic.LoginFingerprint != oldFingerprint {
		t.Fatalf("login fingerprint %v, want the login the prompt was sent with", failed.Diagnostic.LoginFingerprint)
	}
	if warning == nil {
		t.Fatal("a failed prompt must warn before it finishes the turn")
	}
	if !strings.Contains(*warning, "Authentication required") || !core.AuthFailureSignature(core.HarnessKindClaude, *warning) {
		t.Fatal(*warning)
	}

	requests <- acpTestsPromptRequest("second", "still there?")
	var emptyWarning *string
	var completed core.RuntimeEventPromptFinished
	for done := false; !done; {
		switch event := nextEvent().(type) {
		case core.RuntimeEventWarning:
			emptyWarning = &event.Message
		case core.RuntimeEventPromptFinished:
			completed, done = event, true
		}
	}
	// The bridge called this turn a success and produced nothing, so it is
	// reported under its own stop reason rather than as a finished turn (#970).
	if completed.RequestID != "second" || completed.StopReason != core.PromptUnansweredStopReason {
		t.Fatalf("finished %q with %q", completed.RequestID, completed.StopReason)
	}
	if completed.Usage == nil {
		t.Fatal("provider usage must survive ACP completion")
	}
	if completed.Usage.TotalTokens != 30 || completed.Usage.Scope != core.UsageScopeTurn || completed.Usage.ThoughtTokens != nil {
		t.Fatalf("usage %#v", completed.Usage)
	}
	if emptyWarning == nil {
		t.Fatal("an unanswered turn must warn")
	}
	if !strings.Contains(*emptyWarning, core.PromptEmptyResponseMarker) || !strings.Contains(*emptyWarning, "may never have been acted on") {
		t.Fatal(*emptyWarning)
	}

	requests <- acpTestsPromptRequest("third", "answer this")
	sawUpdate := false
	warning = nil
	for done := false; !done; {
		switch event := nextEvent().(type) {
		case core.RuntimeEventSessionUpdate:
			sawUpdate = true
		case core.RuntimeEventWarning:
			warning = &event.Message
		case core.RuntimeEventPromptFinished:
			completed, done = event, true
		}
	}
	if completed.RequestID != "third" || completed.StopReason != "EndTurn" {
		t.Fatalf("finished %q with %q", completed.RequestID, completed.StopReason)
	}
	if !sawUpdate {
		t.Fatal("the scripted response must publish its update")
	}
	if warning != nil {
		t.Fatalf("a response with output must not warn: %s", *warning)
	}

	close(requests)
	if _, err := driver.wait(t, 5*time.Second, "closing the command channel must end the runtime"); err != nil {
		t.Fatalf("a failed prompt must not fail the runtime: %v", err)
	}
	client.Close()
	if count := acpTestsReceive(t, prompts, ioTimeout, "the bridge ends with its input"); count != 3 {
		t.Fatalf("the bridge received %d prompts, want 3", count)
	}
}

// Hard-won: 03e1b5a: Codex model failures counted as completed turns and asked child agents for reports they had not written.
func TestATypedBridgeFailureFailsTheTurnWithTheProvidersReason(t *testing.T) {
	for _, scenario := range []struct {
		harness      core.HarnessKind
		failureKind  *string
		actions      []any
		expectedStop string
	}{
		{core.HarnessKindCodex, nil, []any{"retry"}, promptErrorStopReason},
		{core.HarnessKindClaude, nil, []any{"retry"}, promptErrorStopReason},
		{core.HarnessKindClaude, acpTestsStringPointer("quota_exhausted"), []any{}, core.QuotaStopReason},
		{core.HarnessKindClaude, acpTestsStringPointer("rate_limited"), []any{"retry"}, promptErrorStopReason},
		{core.HarnessKindClaude, acpTestsStringPointer("budget_exhausted"), []any{"new_session"}, promptErrorStopReason},
		{core.HarnessKindClaude, acpTestsStringPointer("context_exhausted"), []any{"new_session"}, promptErrorStopReason},
	} {
		title := `{"type":"error","error":{"message":"model 'gpt-6-luna' is not enabled in rustponsesapi"},"status":400}`
		category := "service"
		if scenario.failureKind != nil {
			title = "This usage limit was reached"
			category = "limit"
		}
		failure := map[string]any{"id": "turn-1:error", "revision": 1,
			"category": category, "severity": "error", "actions": scenario.actions, "title": title}
		client, bridgeStream := acpTestsDuplex(64 * 1024)
		asked := make(chan bool, 1)
		go func() { asked <- typedFailureBridge(t, bridgeStream, failure) }()
		requests := make(chan CommandRequest, 4)
		events := make(chan core.RuntimeEvent, 64)
		spec := reloadFallbackSpec(scenario.harness)
		spec.ResumeSession = nil
		driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)

		requests <- acpTestsPromptRequest("task", "Do the task.")
		warnings := []string{}
		var finished core.RuntimeEventPromptFinished
		for done := false; !done; {
			event, closed, timedOut := acpTestsNext(events, driver, 5*time.Second)
			if timedOut {
				t.Fatal("the runtime keeps reporting")
			}
			if closed {
				t.Fatalf("%v %v: driver ended before failure: %s", scenario.harness, scenario.failureKind, driver.describe())
			}
			switch event := event.(type) {
			case core.RuntimeEventWarning:
				if strings.HasPrefix(event.Message, "prompt") {
					warnings = append(warnings, event.Message)
				}
			case core.RuntimeEventPromptFinished:
				finished, done = event, true
			}
		}
		if finished.StopReason != scenario.expectedStop {
			t.Fatalf("%v %v: stop reason %q, want %q", scenario.harness, scenario.failureKind, finished.StopReason, scenario.expectedStop)
		}
		if finished.Diagnostic == nil {
			t.Fatal("a failed turn carries its diagnostic")
		}
		expectedMessage := "model 'gpt-6-luna' is not enabled in rustponsesapi"
		if scenario.failureKind != nil {
			expectedMessage = title
		}
		if finished.Diagnostic.Message != expectedMessage {
			t.Fatalf("diagnostic message %q, want %q", finished.Diagnostic.Message, expectedMessage)
		}
		if scenario.failureKind == nil {
			if finished.Diagnostic.HTTPStatus == nil || *finished.Diagnostic.HTTPStatus != 400 {
				t.Fatalf("http status %v, want 400", finished.Diagnostic.HTTPStatus)
			}
		} else if finished.Diagnostic.HTTPStatus != nil {
			t.Fatalf("http status %d, want none", *finished.Diagnostic.HTTPStatus)
		}
		label := "prompt failed"
		if scenario.expectedStop == core.QuotaStopReason {
			label = "prompt failed (usage limit reached)"
		}
		if want := []string{label + ": " + expectedMessage}; !slices.Equal(warnings, want) {
			t.Fatalf("warnings %q, want %q", warnings, want)
		}

		close(requests)
		driver.waitBriefly(5 * time.Second)
		driver.abort()
		client.Close()
		if !acpTestsReceive(t, asked, ioTimeout, "the bridge ends with its input") {
			t.Fatal("both adapters are asked for typed failures")
		}
	}
}

// R4-10: Kimi's Bash call carries no `rawInput`. The command exists only as
// the call's streamed text content, `{"command": ...}`, so the form read
// "Kimi Code requests permission: Bash" and nothing else. The recorded
// updates are replayed from the reviewer journal R4 saved
// (`reviewer-B-kimi-bash-permission.jsonl`), followed by a permission request
// that names the call and carries no input, as Kimi's did. Kimi has no
// guardian of its own, and the Mjolnir guardian's reviewer cannot start here
// (there is no `kimi-probe`), so the request reaches the person's form with
// the reason first.
// Hard-won: 11ace4e: Kimi permission forms omitted commands streamed in tool content rather than rawInput.
func TestKimiPermissionFormShowsTheCommandStreamedAsToolContent(t *testing.T) {
	client, agent := acpTestsDuplex(64 * 1024)
	defer agent.Close()
	commands := make(chan CommandRequest, 16)
	events := make(chan core.RuntimeEvent, 256)
	spec := acpTestsSpec(core.HarnessKindKimi, "kimi-probe", "/workspace")
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, commands, events, NewShared[*OpenedSession](nil), false)
	defer driver.abort()
	bridge := acpTestsBridge(agent)
	input := acpTestsLines(t, bridge)
	nextMessage := func() map[string]any {
		t.Helper()
		select {
		case message, ok := <-input:
			if !ok {
				t.Fatal("ACP closed unexpectedly")
			}
			return message
		case <-time.After(5 * time.Second):
			t.Fatal("ACP message timed out")
		}
		return nil
	}
	send := func(message any) {
		t.Helper()
		if !bridge.send(message) {
			t.Fatal("the client stopped reading")
		}
	}

	// Answer the session setup, whatever it asks for, until the prompt.
	commands <- acpTestsPromptRequest("review-1", "review the change")
	for {
		message := nextMessage()
		var result any
		switch message["method"] {
		case "initialize":
			result = map[string]any{"protocolVersion": 1}
		case "session/new":
			result = map[string]any{"sessionId": "kimi-session"}
		case "session/prompt":
		default:
			result = map[string]any{}
		}
		if message["method"] == "session/prompt" {
			break
		}
		if message["id"] != nil {
			send(acpTestsResult(message["id"], result))
		}
	}

	journal, err := os.ReadFile(filepath.Join("testdata", "acp", "kimi_bash_permission.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	var toolCallID *string
	for _, line := range strings.Split(strings.TrimRight(string(journal), "\n"), "\n") {
		event, err := acpTestsDecodeObject([]byte(line))
		if err != nil {
			t.Fatal(err)
		}
		observation := event["observation"]
		if acpTestsGet(observation, "type") != "session_update" {
			continue
		}
		update := acpTestsGet(observation, "data", "update")
		toolCallID = nil
		if id, ok := acpTestsString(acpTestsGet(update, "toolCallId")); ok {
			toolCallID = &id
		}
		send(acpTestsUpdate("kimi-session", update))
	}
	if toolCallID == nil {
		t.Fatal("the journal ends with the call's update")
	}
	send(map[string]any{
		"jsonrpc": "2.0", "id": "permission-1", "method": "session/request_permission",
		"params": map[string]any{
			"sessionId": "kimi-session",
			"toolCall":  map[string]any{"toolCallId": *toolCallID, "title": "Bash", "kind": "execute"},
			"options": []any{
				map[string]any{"optionId": "approve_once", "name": "Approve once", "kind": "allow_once"},
				map[string]any{"optionId": "reject", "name": "Reject", "kind": "reject_once"},
			},
		},
	})

	var message string
	for {
		event := acpTestsExpectEvent(t, events, driver, 5*time.Second, "the permission form must be requested", "runtime stopped")
		if requested, ok := event.(core.RuntimeEventElicitationRequested); ok {
			message = requested.Request.Message
			break
		}
	}
	preface, request, found := strings.Cut(message, "Answer it yourself.\n")
	if !found {
		t.Fatalf("the form names why the guardian handed it over: %s", message)
	}
	if !strings.HasPrefix(preface, "Mjolnir guardian could not decide this request (its review failed: ") {
		t.Fatal(preface)
	}
	if want := "Kimi Code requests permission:\nBash\n$ git status --short && git ls-files && ls -a"; request != want {
		t.Fatalf("request %q, want %q", request, want)
	}
}

// A form the harness withdraws itself (`$/cancel_request` on its
// `elicitation/create`) still resolves as cancelled. Keeping the classifier
// away from open forms must not keep a withdrawn form alive (I1-6).
func TestAFormTheHarnessWithdrawsStillResolves(t *testing.T) {
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	defer bridgeStream.Close()
	go func() {
		bridge := acpTestsBridge(bridgeStream)
		var promptID any
		havePrompt := false
		options := musePolicyOptions()
		for {
			message, ok := bridge.next(t)
			if !ok {
				return
			}
			id := message["id"]
			if id == "ask-1" {
				if !havePrompt {
					t.Errorf("prompt id recorded")
					return
				}
				havePrompt = false
				if !bridge.send(acpTestsResult(promptID, map[string]any{"stopReason": "cancelled"})) {
					return
				}
				continue
			}
			var response any
			switch message["method"] {
			case "initialize":
				response = acpTestsResult(id, map[string]any{"protocolVersion": 1})
			case "session/new":
				response = acpTestsResult(id, map[string]any{"sessionId": "scripted", "configOptions": options})
			case "session/set_config_option":
				params, _ := message["params"].(map[string]any)
				selectOption(options, params)
				response = acpTestsResult(id, map[string]any{"configOptions": options})
			case "session/prompt":
				promptID, havePrompt = id, true
				if !bridge.send(architectureForm()) {
					return
				}
				response = map[string]any{
					"jsonrpc": "2.0",
					"method":  "$/cancel_request",
					"params":  map[string]any{"requestId": "ask-1"},
				}
			default:
				continue
			}
			if !bridge.send(response) {
				return
			}
		}
	}()
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	spec := silentBridgeSpec(core.StallPolicy{})
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
	defer driver.abort()
	requests <- acpTestsPromptRequest("prompt-1", "Ask me which architecture to use")
	deadline := time.Now().Add(5 * time.Second)
	var requested *string
	var resolved core.RuntimeEventElicitationResolved
	for done := false; !done; {
		event, closed, timedOut := acpTestsNext(events, driver, time.Until(deadline))
		if timedOut {
			t.Fatal("a withdrawn form resolves")
		}
		if closed {
			t.Fatalf("runtime stopped: %s", driver.describe())
		}
		switch event := event.(type) {
		case core.RuntimeEventElicitationRequested:
			id := event.Request.ID
			requested = &id
		case core.RuntimeEventElicitationResolved:
			if event.Reply != nil {
				t.Fatal("harness withdrawal is not a user reply")
			}
			resolved, done = event, true
		}
	}
	if requested == nil || *requested != "elicitation-1" {
		t.Fatalf("requested %v", requested)
	}
	if resolved.ElicitationID != "elicitation-1" || resolved.Action != "cancel" {
		t.Fatalf("resolved %#v", resolved)
	}
	close(requests)
	driver.waitBriefly(5 * time.Second)
}

// A Kimi launcher printed installer lines on the bridge's stdout before the
// agent's first frame, and the transport answered each with a parse error
// and never initialized (#1136). Those lines are skipped, not answered.
// Hard-won: 2fe3b29: Kimi installer text on stdout corrupted ACP initialization before its first frame.
func TestLauncherLinesBeforeTheFirstFrameDoNotBreakInitialize(t *testing.T) {
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	// What the client sent. Shared rather than returned: the stdout relay
	// keeps the client stream open, so the bridge never sees end of input.
	var receivedMu sync.Mutex
	var received []map[string]any
	go func() {
		if _, err := bridgeStream.Write([]byte("==> Detected target: linux-x64\n")); err != nil {
			t.Errorf("write the launcher line: %v", err)
			return
		}
		bridge := acpTestsBridge(bridgeStream)
		for {
			message, ok := bridge.next(t)
			if !ok {
				return
			}
			receivedMu.Lock()
			received = append(received, message)
			receivedMu.Unlock()
			id := message["id"]
			var response any
			switch message["method"] {
			case "initialize":
				response = acpTestsResult(id, map[string]any{"protocolVersion": 1})
			case "session/new":
				response = acpTestsResult(id, map[string]any{"sessionId": "scripted"})
			default:
				continue
			}
			if !bridge.send(response) {
				return
			}
		}
	}()
	transport := NewByteStreams(client, skipStdoutPreamble(client))
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	opened := NewShared[*OpenedSession](nil)
	spec := acpTestsSpec(core.HarnessKindKimi, "scripted", acpTestsCurrentDir(t))
	driver := acpTestsStartDrive(transport, spec, requests, events, opened, false)
	acpTestsDrain(events, driver)
	deadline := time.Now().Add(30 * time.Second)
	for {
		session, unlock := opened.Lock()
		isOpen := *session != nil
		unlock()
		if isOpen {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("the session must open despite the launcher's lines")
		}
		time.Sleep(10 * time.Millisecond)
	}
	close(requests)
	driver.abort()
	driver.waitBriefly(ioTimeout)
	bridgeStream.Close()
	receivedMu.Lock()
	defer receivedMu.Unlock()
	if len(received) == 0 || received[0]["method"] != "initialize" {
		t.Fatalf("the bridge first received %v", received)
	}
	for _, message := range received {
		if acpTestsHas(message, "error") {
			t.Fatalf("the client answered launcher output with an error: %v", received)
		}
	}
}

// Hard-won: d045ad87e: historical questions made Jev end unanswered active harness turns.
func TestActiveHarnessTurnIsAuthoritativeEvenAfterProlongedSilence(t *testing.T) {
	classifier, err := acpTestsListen()
	if err != nil {
		t.Fatal(err)
	}
	defer classifier.Close()
	client, bridgeStream := acpTestsDuplex(128 * 1024)
	started := make(chan struct{}, 1)
	release := make(chan struct{}, 1)
	bridgeDone := acpTestsGo(func() {
		bridge := acpTestsBridge(bridgeStream)
		gate := true
		for {
			request, ok := bridge.next(t)
			if !ok {
				return
			}
			method, _ := acpTestsString(request["method"])
			var result any
			switch method {
			case "initialize":
				result = map[string]any{"protocolVersion": 1}
			case "session/new":
				result = map[string]any{"sessionId": "scripted", "modes": map[string]any{
					"currentModeId": "auto", "availableModes": []any{map[string]any{"id": "auto", "name": "Auto"}},
				}}
			case "session/set_mode":
				result = map[string]any{}
			case "session/prompt":
				if !gate {
					t.Errorf("a second prompt reached the bridge")
					return
				}
				gate = false
				started <- struct{}{}
				<-release
				if !bridge.send(acpTestsUpdate("scripted", map[string]any{"sessionUpdate": "agent_message_chunk",
					"content": map[string]any{"type": "text", "text": "Here is the requested turn review."}})) {
					t.Errorf("write the reply")
					return
				}
				result = map[string]any{"stopReason": "end_turn"}
			default:
				continue
			}
			if !bridge.send(acpTestsResult(request["id"], result)) {
				t.Errorf("write the response")
				return
			}
		}
	})
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	spec := silentBridgeSpec(core.StallPolicy{})
	spec.Harness = core.HarnessKindClaude
	spec.Verdict = VerdictSourceHosted{Endpoint: fmt.Sprintf("http://%s/", classifier.Addr())}
	turnContext := spec.TurnContext
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
	defer driver.abort()
	requests <- acpTestsPromptRequest("prompt-turn-review", "fable, turn review pls")
	acpTestsReceive(t, started, 10*time.Second, "the prompt reaches the bridge")
	// Cross both the old 60-second question check and 40-minute finish rule.
	turnContext.MarkParentActivityAt(time.Now().Add(-41 * time.Minute))
	if conn, err := acpTestsAcceptWithin(classifier, 2*time.Second); err == nil {
		conn.Close()
		t.Fatal("an active harness turn must not be submitted for classifier completion")
	}
	for {
		event, ok := acpTestsTryRecv(events)
		if !ok {
			break
		}
		if _, finished := event.(core.RuntimeEventPromptFinished); finished {
			t.Fatalf("premature completion: %#v", event)
		}
	}
	release <- struct{}{}
	deadline := time.Now().Add(10 * time.Second)
	sawReply := false
	for done := false; !done; {
		event := acpTestsExpectEvent(t, events, driver, time.Until(deadline), "the turn finishes", "the runtime stopped")
		if update, ok := acpTestsUpdateValue(t, event); ok && acpTestsGet(update, "content", "text") == "Here is the requested turn review." {
			sawReply = true
		}
		if finished, ok := event.(core.RuntimeEventPromptFinished); ok {
			if !sawReply {
				t.Fatal("the reply arrives before the turn finishes")
			}
			if finished.RequestID != "prompt-turn-review" || finished.StopReason != "EndTurn" {
				t.Fatalf("finished %q with %q", finished.RequestID, finished.StopReason)
			}
			done = true
		}
	}
	close(requests)
	if _, err := driver.wait(t, 10*time.Second, "the runtime ends with its requests"); err != nil {
		t.Fatal(err)
	}
	client.Close()
	acpTestsReceive(t, bridgeDone, 10*time.Second, "the bridge ends with its input")
}

func TestNativeChildLoadNegotiatesAndRoutesHistoryForClaudeAndCodex(t *testing.T) {
	for _, scenario := range []struct {
		harness core.HarnessKind
		missing bool
	}{
		{core.HarnessKindClaude, false},
		{core.HarnessKindCodex, false},
		{core.HarnessKindCodex, true},
	} {
		harness, missing := scenario.harness, scenario.missing
		client, bridgeStream := acpTestsDuplex(64 * 1024)
		go func() {
			bridge := acpTestsBridge(bridgeStream)
			for {
				request, ok := bridge.next(t)
				if !ok {
					return
				}
				method, _ := acpTestsString(request["method"])
				var result any
				switch method {
				case "initialize":
					expected := []any{"asyncTasks", "nativeSubagentSessions", "sessionFailure"}
					if harness == core.HarnessKindClaude {
						expected = append(expected, "backgroundSubagents")
					}
					if got := acpTestsGet(request, "params", "clientCapabilities", "_meta", "jetbrains", "air", "capabilities"); !acpTestsJSONEqual(t, got, expected) {
						t.Errorf("%v advertised %v, want %v", harness, got, expected)
						return
					}
					result = map[string]any{"protocolVersion": 1, "agentCapabilities": map[string]any{"sessionCapabilities": map[string]any{"resume": map[string]any{}}},
						"_meta": map[string]any{"jetbrains": map[string]any{"air": map[string]any{"version": 1, "capabilities": []any{"nativeSubagentSessions"}}}}}
				case "session/load", "session/new":
					want := "session/load"
					if missing {
						want = "session/new"
					}
					if method != want {
						t.Errorf("%v opened with %s, want %s", harness, method, want)
						return
					}
					// Exceed the pipe capacity so the driver must drain history
					// concurrently with the load response.
					for _, update := range []struct {
						address string
						update  map[string]any
					}{
						{"root", map[string]any{"sessionUpdate": "subagent_spawned", "subagentSessionId": "child", "name": "review", "task": "inspect", "capabilities": map[string]any{}}},
						{"child", map[string]any{"sessionUpdate": "agent_message_chunk", "content": map[string]any{"type": "text", "text": strings.Repeat("x", 80000)}}},
						{"root", map[string]any{"sessionUpdate": "subagent_state_update", "subagentSessionId": "child", "state": "completed"}},
					} {
						if !bridge.send(acpTestsUpdate(update.address, update.update)) {
							t.Errorf("write child history")
							return
						}
					}
					result = map[string]any{"sessionId": "replacement", "modes": map[string]any{"currentModeId": "auto", "availableModes": []any{
						map[string]any{"id": "auto", "name": "Auto"},
						map[string]any{"id": "agent", "name": "Agent"},
					}}}
				case "session/resume":
					t.Errorf("native recovery must load child history")
					return
				case "session/set_mode", "session/set_config_option":
					result = map[string]any{}
				default:
					continue
				}
				if !bridge.send(acpTestsResult(request["id"], result)) {
					t.Errorf("write the response")
					return
				}
			}
		}()
		requests := make(chan CommandRequest, 1)
		events := make(chan core.RuntimeEvent, 32)
		spec := acpTestsSpec(harness, "scripted", acpTestsCurrentDir(t))
		spec.ResumeSession = acpTestsStringPointer("root")
		if missing {
			spec.NativeSessionHistory = NativeSessionHistoryCreatedHereUnused
		} else {
			spec.NativeSessionHistory = NativeSessionHistoryMayHaveHistory
		}
		driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)

		var children []core.NativeAgentEvent
		missingBackgroundCapability := false
		deadline := time.Now().Add(10 * time.Second)
		for configured := false; !configured; {
			event, closed, timedOut := acpTestsNext(events, driver, time.Until(deadline))
			if timedOut {
				t.Fatal("child history load finishes")
			}
			if closed {
				break
			}
			switch event := event.(type) {
			case core.RuntimeEventNativeAgent:
				children = append(children, event.Event)
			case core.RuntimeEventWarning:
				if strings.Contains(event.Message, "backgroundSubagents") {
					if !strings.Contains(event.Message, "0.95.0") || !strings.Contains(event.Message, "upgrade") {
						t.Fatal(event.Message)
					}
					missingBackgroundCapability = true
				}
			case core.RuntimeEventSessionConfigured:
				configured = true
			case core.RuntimeEventSessionUpdate:
				if update, _ := acpTestsUpdateValue(t, event); acpTestsGet(update, "sessionUpdate") == "agent_message_chunk" {
					t.Fatalf("child history reached the root transcript: %v", update)
				}
			}
		}
		if missingBackgroundCapability != (harness == core.HarnessKindClaude) {
			t.Fatalf("%v: missing background capability reported %v", harness, missingBackgroundCapability)
		}
		if len(children) != 5 {
			t.Fatalf("%v missing=%v: native agent events %#v", harness, missing, children)
		}
		spawnIndex := 1
		if missing {
			// Hard-won: 14b4b00de: replacing an unused thread skipped the replay boundary, so staging from an abandoned load hid later native children.
			_, begin := children[0].(core.NativeAgentEventReplayBegin)
			_, commit := children[1].(core.NativeAgentEventReplayCommit)
			if !begin || !commit {
				t.Fatalf("an abandoned load still closes its replay: %#v", children)
			}
			spawnIndex = 2
		} else {
			_, begin := children[0].(core.NativeAgentEventReplayBegin)
			_, commit := children[4].(core.NativeAgentEventReplayCommit)
			if !begin || !commit {
				t.Fatalf("the load's history is one replay: %#v", children)
			}
		}
		if _, ok := children[spawnIndex].(core.NativeAgentEventSpawned); !ok {
			t.Fatalf("event %d is the spawn: %#v", spawnIndex, children)
		}
		if _, ok := children[spawnIndex+1].(core.NativeAgentEventUpdate); !ok {
			t.Fatalf("event %d is the child's update: %#v", spawnIndex+1, children)
		}
		if _, ok := children[spawnIndex+2].(core.NativeAgentEventState); !ok {
			t.Fatalf("event %d is the child's state: %#v", spawnIndex+2, children)
		}
		close(requests)
		if _, err := driver.wait(t, ioTimeout, "the runtime ends with its requests"); err != nil {
			t.Fatal(err)
		}
		bridgeStream.Close()
	}
}

// When the profile's OpenCode logins change, the session reloads on a fresh
// bridge to import them: never in the middle of a turn, and always the same
// native session.
func TestOpencodeReloadsItsSessionForChangedLoginsOnlyBetweenTurns(t *testing.T) {
	root := t.TempDir()
	home := filepath.Join(root, "profile", "opencode")
	logins := filepath.Join(home, core.HarnessKindOpenCode.CredentialFileName())
	if err := os.MkdirAll(filepath.Dir(logins), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(logins, []byte(`[{"id":"cred_first"}]`), 0o600); err != nil {
		t.Fatal(err)
	}
	environment := map[string]string{
		"XDG_CONFIG_HOME": filepath.Join(root, "profile"),
		"XDG_DATA_HOME":   filepath.Join(home, ".data"),
	}
	spec := reloadFallbackSpec(core.HarnessKindOpenCode)
	spec.ResumeSession = nil
	imported, err := importedLoginsPrepare(environment)
	if err != nil {
		t.Fatal(err)
	}
	spec.ImportedLogins = &imported
	spec.Environment = environment

	observed := make(chan string, 256)
	release := make(chan struct{}, 16)
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	go heldPromptBridge(t, bridgeStream, observed, release)
	defer bridgeStream.Close()
	requests := make(chan CommandRequest, 1)
	events := make(chan core.RuntimeEvent, 256)
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
	defer driver.abort()
	acpTestsWaitFor(t, observed, "session/new")
	requests <- acpTestsPromptRequest("turn", "work")
	acpTestsWaitFor(t, observed, "session/prompt")

	// The logins change while the turn runs; the turn is left alone.
	if err := os.WriteFile(logins, []byte(`[{"id":"cred_second"}]`), 0o600); err != nil {
		t.Fatal(err)
	}
	time.Sleep(checkInterval * 10)
	if driver.isFinished() {
		t.Fatalf("a reload interrupted the turn: %s", driver.describe())
	}

	release <- struct{}{}
	restart, err := driver.wait(t, 5*time.Second, "the idle session reloads")
	if err != nil {
		t.Fatalf("the reload is not an error: %v", err)
	}
	if reload, ok := restart.(SessionRestartReloadLogins); !ok || reload.Value != "ses_opencode" {
		t.Fatalf("restart %#v", restart)
	}
	finished := false
	for {
		event, ok := acpTestsTryRecv(events)
		if !ok {
			break
		}
		if prompt, ok := event.(core.RuntimeEventPromptFinished); ok && prompt.RequestID == "turn" {
			finished = true
		}
	}
	if !finished {
		t.Fatal("the turn finished before the reload")
	}
}

func TestCodexStillFailsWhenTheRecordedSessionCannotBeReloaded(t *testing.T) {
	observed := make(chan string, 64)
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	go sessionReloadRejectingBridge(t, bridgeStream, observed, false)
	defer bridgeStream.Close()
	requests := make(chan CommandRequest, 1)
	events := make(chan core.RuntimeEvent, 32)
	driver := acpTestsStartDrive(acpTestsTransport(client), reloadFallbackSpec(core.HarnessKindCodex), requests, events, NewShared[*OpenedSession](nil), false)

	_, err := driver.wait(t, 5*time.Second, "a failed load ends the runtime")
	if err == nil {
		t.Fatal("a harness with restorable native state must not start fresh")
	}
	if !strings.Contains(err.Error(), "load ACP session gone") {
		t.Fatalf("unexpected error: %v", err)
	}
	var methods []string
	for {
		method, ok := acpTestsTryRecv(observed)
		if !ok {
			break
		}
		if method == "session/new" || method == "session/load" || method == "session/resume" {
			methods = append(methods, method)
		}
	}
	if !slices.Equal(methods, []string{"session/load"}) {
		t.Fatalf("session opens %q", methods)
	}
}

func TestResumeFailuresReportAMissingNativeSessionPerHarness(t *testing.T) {
	spec := func(harness core.HarnessKind) *LaunchSpec {
		spec := acpTestsSpec(harness, "agent", "/workspace/app")
		spec.ResumeSession = acpTestsStringPointer("native")
		return &spec
	}
	// The message as codex-acp wraps it.
	missingThread := fmt.Errorf("resume ACP session native: %w",
		errors.New(`Internal error: {"details": "no rollout found for thread id native"}`))
	// The ACP standard resource error as claude-agent-acp answers it.
	missingTranscript := fmt.Errorf("resume ACP session native: %w",
		acpDisplayError{err: acpTestsRPCError(-32002, "Resource not found", map[string]any{"sessionId": "native"})})
	if !harnessReportsMissingNativeSession(spec(core.HarnessKindCodex), missingThread) {
		t.Fatal("Codex recognizes its missing thread")
	}
	if !harnessReportsMissingNativeSession(spec(core.HarnessKindClaude), missingTranscript) {
		t.Fatal("Claude recognizes its missing transcript")
	}
	// Each harness only recognizes its own wording.
	if harnessReportsMissingNativeSession(spec(core.HarnessKindClaude), missingThread) {
		t.Fatal("Claude does not read Codex's wording")
	}
	if harnessReportsMissingNativeSession(spec(core.HarnessKindCodex), missingTranscript) {
		t.Fatal("Codex does not read Claude's error")
	}
	// A harness that always materializes its session never reports one gone.
	if harnessReportsMissingNativeSession(spec(core.HarnessKindKimi), missingThread) {
		t.Fatal("Kimi never reports a missing session")
	}
	// Any other failure keeps failing the resume.
	if harnessReportsMissingNativeSession(spec(core.HarnessKindCodex), errors.New("Internal error: session store is locked")) {
		t.Fatal("a locked store is not a missing thread")
	}
	if harnessReportsMissingNativeSession(spec(core.HarnessKindClaude), errors.New("resume ACP session native: connection closed")) {
		t.Fatal("a closed connection is not a missing transcript")
	}
}

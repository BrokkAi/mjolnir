//go:build unix

// Port of the #[cfg(unix)] items of mj-worker/src/acp/claude_result_tests.rs:
// the runtime's events recorded into a real durable relay.

package worker

import (
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type claudeRelayPeer struct {
	probe    *claudeProbe
	relay    *RelayCell
	inFlight map[string]core.RelayCommand
}

func newClaudeRelayPeer(t *testing.T) *claudeRelayPeer {
	t.Helper()
	durable, err := DurableRelayOpen(t.TempDir(), claudeSession, "test")
	if err != nil {
		t.Fatal(err)
	}
	durable.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
	durable.SetBackgroundWorkPolicy(BackgroundWorkPolicyAirTasks)
	peer := &claudeRelayPeer{
		probe:    newClaudeProbe(t),
		relay:    NewRelayCell(durable),
		inFlight: map[string]core.RelayCommand{},
	}
	for {
		if _, configured := peer.event().(core.RuntimeEventSessionConfigured); configured {
			break
		}
	}
	return peer
}

func (p *claudeRelayPeer) event() core.RuntimeEvent {
	p.probe.t.Helper()
	event := p.probe.event()
	if _, err := recordRuntimeEvent(p.relay, p.inFlight, event); err != nil {
		p.probe.t.Fatal(err)
	}
	return event
}

func (p *claudeRelayPeer) prompt() map[string]any {
	p.probe.t.Helper()
	command := core.RelayCommandPrompt{Prompt: textPrompt("start background work")}
	relay := p.relay.Lock()
	submitRelay(relay.DurableRelay, "prompt-1", command)
	claimed, err := relay.ClaimPendingCommands(true)
	relay.Unlock()
	if err != nil || len(claimed) != 1 {
		p.probe.t.Fatalf("claimed = %#v, err = %v", claimed, err)
	}
	p.inFlight["prompt-1"] = command
	return p.probe.prompt("prompt-1", "start background work")
}

func (p *claudeRelayPeer) finished() {
	p.probe.t.Helper()
	for {
		if _, finished := p.event().(core.RuntimeEventPromptFinished); finished {
			return
		}
	}
}

func (p *claudeRelayPeer) waitFor(matches func(core.RuntimeEvent) bool) {
	p.probe.t.Helper()
	for !matches(p.event()) {
	}
}

func isClaudeTurnResult(event core.RuntimeEvent) bool {
	_, ok := event.(core.RuntimeEventClaudeTurnResult)
	return ok
}

func isAirTaskChanged(event core.RuntimeEvent) bool {
	_, ok := event.(core.RuntimeEventAirTaskChanged)
	return ok
}

func (p *claudeRelayPeer) taskCompleted(taskID string) {
	p.probe.t.Helper()
	p.probe.send(map[string]any{"jsonrpc": "2.0", "method": "session/update", "params": map[string]any{
		"sessionId": claudeSession, "update": map[string]any{"sessionUpdate": "async_task_state_update",
			"asyncTaskId": taskID, "state": "completed"},
	}})
}

// Notification ingress can outrun the prompt loop even though the adapter
// emits the response first. The relay restores that order before queue
// claims.
func TestAnAutonomousSpawnWaitsForThePromptBoundaryUnlessItAlreadyEnded(t *testing.T) {
	for _, endedFirst := range []bool{false, true} {
		peer := newClaudeRelayPeer(t)
		prompt := peer.prompt()
		peer.probe.chunk("Started background work.")
		peer.probe.sdkResult(claudeSuccess("human"))
		peer.waitFor(isClaudeTurnResult)
		{
			relay := peer.relay.Lock()
			state := relay.OperationalState()
			relay.Unlock()
			if state.ActivePrompt == nil || state.HarnessTurn != nil || state.Execution != core.RelayExecutionStateRunning {
				t.Fatalf("state = %+v", state)
			}
		}
		peer.probe.autonomousTurn("next-turn")
		peer.waitFor(isAirTaskChanged)
		if endedFirst {
			peer.taskCompleted("next-turn")
			peer.waitFor(isAirTaskChanged)
		}
		{
			relay := peer.relay.Lock()
			if relay.OperationalState().HarnessTurn != nil {
				t.Fatal("no harness turn starts inside the prompt")
			}
			submitRelay(relay.DurableRelay, "queued-prompt", core.RelayCommandPrompt{Prompt: textPrompt("next request")})
			claimed, err := relay.ClaimPendingCommands(true)
			if err != nil || len(claimed) != 0 {
				t.Fatalf("claimed = %#v, err = %v", claimed, err)
			}
			controls := relay.OperationalState().TurnControls
			relay.Unlock()
			if controls == nil || !reflect.DeepEqual(controls.Interrupt, core.RelayCommand(core.RelayCommandCancelTurn{})) ||
				controls.Primary == nil || !reflect.DeepEqual(controls.Primary.Command, core.RelayCommand(core.RelayCommandCancelTurn{})) {
				t.Fatalf("controls = %#v", controls)
			}
		}
		peer.probe.result(prompt, map[string]any{"stopReason": "end_turn"})
		peer.finished()
		{
			relay := peer.relay.Lock()
			if (relay.OperationalState().HarnessTurn != nil) != !endedFirst {
				t.Fatalf("harness turn = %v, ended first = %v", relay.OperationalState().HarnessTurn, endedFirst)
			}
			events, err := relay.EventsAfter(0, core.RelayEventGenesisDigest)
			if err != nil {
				t.Fatal(err)
			}
			boundary, start := -1, -1
			for i, event := range events {
				switch observation := event.Observation.(type) {
				case core.RelayObservationCommandCompleted:
					if observation.CommandID == "prompt-1" && boundary < 0 {
						boundary = i
					}
				case core.RelayObservationHarnessTurnStarted:
					if start < 0 {
						start = i
					}
				}
			}
			if boundary < 0 {
				t.Fatal("the prompt completed")
			}
			claimed, err := relay.ClaimPendingCommands(true)
			if err != nil {
				t.Fatal(err)
			}
			if endedFirst {
				if start >= 0 {
					t.Fatal("no harness turn started")
				}
				if len(claimed) == 0 || claimed[0].CommandID != "queued-prompt" {
					t.Fatalf("claimed = %#v", claimed)
				}
			} else {
				if start <= boundary {
					t.Fatalf("start = %d, boundary = %d", start, boundary)
				}
				if len(claimed) != 0 {
					t.Fatalf("claimed = %#v", claimed)
				}
			}
			relay.Unlock()
		}
		if !endedFirst {
			peer.probe.chunk("Autonomous answer")
			peer.probe.sdkResult(claudeSuccess("task-notification"))
			peer.waitFor(isClaudeTurnResult)
			relay := peer.relay.Lock()
			claimed, err := relay.ClaimPendingCommands(true)
			relay.Unlock()
			if err != nil || len(claimed) != 0 {
				t.Fatalf("claimed = %#v, err = %v", claimed, err)
			}
			peer.taskCompleted("next-turn")
			peer.waitFor(isAirTaskChanged)
			relay = peer.relay.Lock()
			claimed, err = relay.ClaimPendingCommands(true)
			relay.Unlock()
			if err != nil || len(claimed) == 0 || claimed[0].CommandID != "queued-prompt" {
				t.Fatalf("claimed = %#v, err = %v", claimed, err)
			}
		}
		peer.probe.close()
	}
}

func TestANativeChildCanSendOutputAndRequestPermissionAfterThePromptResponse(t *testing.T) {
	peer := newClaudeRelayPeer(t)
	prompt := peer.prompt()
	peer.probe.send(map[string]any{"jsonrpc": "2.0", "method": "session/update", "params": map[string]any{
		"sessionId": claudeSession, "update": map[string]any{"sessionUpdate": "subagent_spawned", "subagentSessionId": "child",
			"name": "review", "task": "inspect source", "capabilities": map[string]any{}},
	}})
	peer.probe.chunk("The child is reviewing.")
	peer.probe.result(prompt, map[string]any{"stopReason": "end_turn"})
	peer.finished()
	childState := func() core.NativeAgentState {
		relay := peer.relay.Lock()
		defer relay.Unlock()
		state := relay.OperationalState()
		if state.ActivePrompt != nil {
			t.Fatal("the prompt ended")
		}
		for _, agent := range state.NativeAgents {
			if agent.SessionID == "child" {
				return agent.State
			}
		}
		t.Fatal("the child is known")
		return ""
	}
	if state := childState(); state != core.NativeAgentStateRunning {
		t.Fatalf("child = %v", state)
	}
	peer.probe.send(map[string]any{"jsonrpc": "2.0", "method": "session/update", "params": map[string]any{
		"sessionId": "child", "update": map[string]any{"sessionUpdate": "agent_message_chunk",
			"content": map[string]any{"type": "text", "text": "Still reviewing."}},
	}})
	peer.waitFor(func(event core.RuntimeEvent) bool {
		native, ok := event.(core.RuntimeEventNativeAgent)
		if !ok {
			return false
		}
		_, update := native.Event.(core.NativeAgentEventUpdate)
		return update
	})
	peer.probe.send(map[string]any{"jsonrpc": "2.0", "id": "child-permission", "method": "session/request_permission",
		"params": map[string]any{"sessionId": "child", "toolCall": map[string]any{"toolCallId": "child-read", "title": "Read source", "kind": "read"},
			"options": []any{map[string]any{"optionId": "approve", "name": "Approve once", "kind": "allow_once"},
				map[string]any{"optionId": "reject", "name": "Reject", "kind": "reject_once"}}},
	})
	var request core.ElicitationRequest
	peer.waitFor(func(event core.RuntimeEvent) bool {
		requested, ok := event.(core.RuntimeEventElicitationRequested)
		request = requested.Request
		return ok
	})
	if !strings.Contains(request.Message, "child") {
		t.Fatalf("message = %s", request.Message)
	}
	// Hard-won: 509af374e: a child's call ID can match an unrelated parent row.
	if request.ToolPermission.ToolCallID != nil {
		t.Fatalf("tool call id = %v", *request.ToolPermission.ToolCallID)
	}
	if err := peer.probe.resolve(request.ID, core.ElicitationResponse{Action: core.ElicitationResponseDecline}); err != nil {
		t.Fatal(err)
	}
	answer := peer.probe.message()
	if answer["id"] != "child-permission" || jsonAt(answer, "result", "outcome", "optionId") != "reject" {
		t.Fatalf("answer = %v", answer)
	}
	{
		relay := peer.relay.Lock()
		state := relay.OperationalState()
		relay.Unlock()
		if state.ActivePrompt != nil || state.HarnessTurn != nil {
			t.Fatalf("state = %+v", state)
		}
	}
	if state := childState(); state != core.NativeAgentStateRunning {
		t.Fatalf("child = %v", state)
	}
	peer.probe.close()
}

// #1137: the cancellation acknowledgement and an SDK cycle result can arrive
// while Claude's prompt is still running. Only the prompt response lets the
// worker claim the Move's checkpoint; the controller needs no idle guess.
func TestAMoveCheckpointWaitsForTheCancelledClaudePromptResponse(t *testing.T) {
	durable, err := DurableRelayOpen(t.TempDir(), claudeSession, "test")
	if err != nil {
		t.Fatal(err)
	}
	durable.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
	relay := NewRelayCell(durable)
	inFlight := map[string]core.RelayCommand{}
	probe := newClaudeProbe(t)
	record := func(event core.RuntimeEvent) {
		t.Helper()
		if _, err := recordRuntimeEvent(relay, inFlight, event); err != nil {
			t.Fatal(err)
		}
	}
	for {
		event := probe.event()
		_, configured := event.(core.RuntimeEventSessionConfigured)
		record(event)
		if configured {
			break
		}
	}
	command := core.RelayCommandPrompt{Prompt: textPrompt("go")}
	{
		guard := relay.Lock()
		submitRelay(guard.DurableRelay, "prompt-1", command)
		claimed, err := guard.ClaimPendingCommands(true)
		if err != nil || len(claimed) == 0 || claimed[0].CommandID != "prompt-1" {
			t.Fatalf("claimed = %#v, err = %v", claimed, err)
		}
		reason := "Move"
		submitRelay(guard.DurableRelay, "move-cut", core.RelayCommandBeginCheckpoint{Reason: &reason})
		submitRelay(guard.DurableRelay, "cancel-1", core.RelayCommandCancelTurn{})
		claimed, err = guard.ClaimPendingCommands(true)
		if err != nil || len(claimed) != 1 || claimed[0].CommandID != "cancel-1" {
			t.Fatalf("claimed = %#v, err = %v", claimed, err)
		}
		guard.Unlock()
	}
	inFlight["prompt-1"] = command
	inFlight["cancel-1"] = core.RelayCommandCancelTurn{}
	prompt := probe.prompt("prompt-1", "go")
	probe.command(CommandRequestCancel{RequestID: "cancel-1"})
	if message := probe.message(); message["method"] != "session/cancel" {
		t.Fatalf("message = %v", message)
	}
	probe.sdkResult(claudeSuccess("human"))
	// Cross the pipe buffer boundary while the cancelled prompt still owns
	// work.
	probe.chunk(strings.Repeat("still working ", 8192))
	acknowledged, cycleResult, outputReceived := false, false, false
	for !(acknowledged && cycleResult && outputReceived) {
		event := probe.event()
		switch event := event.(type) {
		case core.RuntimeEventCancelApplied:
			acknowledged = true
		case core.RuntimeEventClaudeTurnResult:
			cycleResult = true
		case core.RuntimeEventSessionUpdate:
			if jsonAt(decodeJSONValue(t, event.Update), "sessionUpdate") == "agent_message_chunk" {
				outputReceived = true
			}
		case core.RuntimeEventPromptFinished:
			t.Fatal("the cancelled prompt has not answered")
		}
		record(event)
		guard := relay.Lock()
		claimed, err := guard.ClaimPendingCommands(true)
		state := guard.OperationalState()
		guard.Unlock()
		if err != nil || len(claimed) != 0 || state.CheckpointReady != nil || state.ActivePrompt == nil {
			t.Fatalf("claimed = %#v, err = %v, state = %+v", claimed, err, state)
		}
	}
	probe.result(prompt, map[string]any{"stopReason": "cancelled"})
	for {
		event := probe.event()
		_, finished := event.(core.RuntimeEventPromptFinished)
		record(event)
		if finished {
			break
		}
	}
	{
		guard := relay.Lock()
		claimed, err := guard.ClaimPendingCommands(true)
		if err != nil || len(claimed) != 1 || claimed[0].CommandID != "move-cut" {
			t.Fatalf("claimed = %#v, err = %v", claimed, err)
		}
		if _, err := guard.RecordCheckpointReady("move-cut"); err != nil {
			t.Fatal(err)
		}
		state := guard.OperationalState()
		guard.Unlock()
		if state.CheckpointReady == nil || state.ActivePrompt != nil {
			t.Fatalf("state = %+v", state)
		}
	}
	probe.close()
}

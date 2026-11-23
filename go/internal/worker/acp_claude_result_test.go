// Port of mj-worker/src/acp/claude_result_tests.rs.

// Claude foreground prompts end only at PromptResponse; raw results still
// serve autonomous turns.

package worker

import (
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const claudeSession = "claude-session"

type claudeProbe struct {
	*acpProbe
}

func newClaudeProbe(t *testing.T) *claudeProbe {
	t.Helper()
	return claudeProbeWithReplayedAutonomousTurn(t, false)
}

func claudeProbeWithReplayedAutonomousTurn(t *testing.T, replay bool) *claudeProbe {
	t.Helper()
	spec := acpDriveSpec("claude-probe", core.HarnessKindClaude, core.ExecutionPolicyConfiguredApprovals, "/workspace")
	// These probes exercise the person's permission form.
	spec.ApprovalChoices = core.DefaultApprovalChoices()
	spec.ApprovalChoices.MjolnirGuardian = false
	if replay {
		session := claudeSession
		spec.ResumeSession = &session
	}
	probe := &claudeProbe{startACPProbe(t, spec, 64*1024)}
	initialize := probe.message()
	if initialize["method"] != "initialize" {
		t.Fatalf("first message = %v", initialize)
	}
	capabilities, _ := jsonAt(initialize, "params", "clientCapabilities", "_meta", "jetbrains", "air", "capabilities").([]any)
	if !slices.Contains(capabilities, any("backgroundSubagents")) {
		t.Fatalf("capabilities = %v", capabilities)
	}
	probe.result(initialize, map[string]any{
		"protocolVersion": 1, "agentCapabilities": map[string]any{"loadSession": true},
		"_meta": map[string]any{"steering": map[string]any{"supported": true}, "jetbrains": map[string]any{
			"air": map[string]any{"capabilities": []any{"nativeSubagentSessions", "backgroundSubagents"}}}},
	})
	opening := probe.message()
	want := "session/new"
	if replay {
		want = "session/load"
	}
	if opening["method"] != want {
		t.Fatalf("open = %v", opening)
	}
	if !replay && !jsonEqual(jsonAt(opening, "params", "_meta", "claudeCode", "emitRawSDKMessages"),
		[]any{map[string]any{"type": "result"}}) {
		t.Fatalf("open = %v", opening)
	}
	if replay {
		probe.autonomousTurn("replayed-turn")
	}
	probe.result(opening, map[string]any{"sessionId": claudeSession, "modes": map[string]any{
		"currentModeId": "default", "availableModes": []any{
			map[string]any{"id": "default", "name": "Default"}, map[string]any{"id": "auto", "name": "Auto"},
		},
	}})
	mode := probe.message()
	if mode["method"] != "session/set_mode" {
		t.Fatalf("mode = %v", mode)
	}
	probe.result(mode, map[string]any{})
	return probe
}

func (p *claudeProbe) chunk(text string) {
	p.t.Helper()
	p.send(map[string]any{
		"jsonrpc": "2.0",
		"method":  "session/update",
		"params": map[string]any{"sessionId": claudeSession, "update": map[string]any{
			"sessionUpdate": "agent_message_chunk",
			"content":       map[string]any{"type": "text", "text": text},
		}},
	})
}

func (p *claudeProbe) autonomousTurn(taskID string) {
	p.t.Helper()
	p.send(map[string]any{
		"jsonrpc": "2.0", "method": "session/update",
		"params": map[string]any{"sessionId": claudeSession, "update": map[string]any{
			"sessionUpdate": "async_task_spawned", "asyncTaskId": taskID,
			"name": "Claude is working", "taskType": "turn",
			"showInTranscript": false, "canStop": true,
		}},
	})
}

func (p *claudeProbe) sdkResult(message any) {
	p.t.Helper()
	p.send(map[string]any{
		"jsonrpc": "2.0",
		"method":  "_claude/sdkMessage",
		"params":  map[string]any{"sessionId": claudeSession, "message": message},
	})
}

func (p *claudeProbe) prompt(requestID, text string) map[string]any {
	p.t.Helper()
	p.command(CommandRequestPrompt{RequestID: requestID, Prompt: textPrompt(text)})
	prompt := p.message()
	if prompt["method"] != "session/prompt" {
		p.t.Fatalf("prompt = %v", prompt)
	}
	return prompt
}

// finished waits until requestID finishes, and returns its stop reason and
// usage. Any other prompt finishing first fails the test.
func (p *claudeProbe) finished(requestID string) (string, *core.TokenUsage) {
	p.t.Helper()
	for {
		if finished, ok := p.event().(core.RuntimeEventPromptFinished); ok {
			if finished.RequestID != requestID {
				p.t.Fatalf("the wrong prompt finished: %s", finished.RequestID)
			}
			return finished.StopReason, finished.Usage
		}
	}
}

func (p *claudeProbe) close() {
	p.t.Helper()
	p.command(CommandRequestClose{RequestID: "close"})
	var closing map[string]any
	for {
		message := p.message()
		if message["method"] == "$/cancel_request" {
			p.t.Fatalf("unexpected cancellation: %v", message)
		}
		if message["method"] == "session/close" {
			closing = message
			break
		}
	}
	p.result(closing, map[string]any{})
	restart, err := p.outcome()
	if err != nil {
		p.t.Fatalf("close succeeds: %v", err)
	}
	if restart != nil {
		p.t.Fatalf("restart = %#v", restart)
	}
}

func claudeSuccess(origin string) map[string]any {
	return map[string]any{
		"type":        "result",
		"subtype":     "success",
		"is_error":    false,
		"num_turns":   2,
		"stop_reason": "end_turn",
		"result":      "done",
		"usage": map[string]any{
			"input_tokens":                100,
			"output_tokens":               7,
			"cache_read_input_tokens":     1000,
			"cache_creation_input_tokens": 10,
		},
		"origin": map[string]any{"kind": origin},
	}
}

// claudeSteerInterruption is the report of a cycle that a steer
// interrupted.
func claudeSteerInterruption() map[string]any {
	return map[string]any{
		"type":              "result",
		"subtype":           "error_during_execution",
		"is_error":          true,
		"num_turns":         1,
		"stop_reason":       nil,
		"errors":            []any{"[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"},
		"usage":             map[string]any{"input_tokens": 50, "output_tokens": 3},
		"origin":            map[string]any{"kind": "human"},
		"queued_turn_count": 1,
	}
}

// Hard-won: 53fdab4c1: Claude native sub-agent usage was discarded with the prompt response metadata.
func TestClaudePromptUsageIncludesNativeAgentsWithoutRecountingTheParent(t *testing.T) {
	probe := newClaudeProbe(t)
	prompt := probe.prompt("with-agents", "delegate to Sonnet")
	probe.chunk("Done.")
	probe.result(prompt, map[string]any{
		"stopReason": "end_turn",
		"usage": map[string]any{"totalTokens": 1117, "inputTokens": 100, "outputTokens": 7,
			"cachedReadTokens": 1000, "cachedWriteTokens": 10},
		"_meta": map[string]any{"quota": map[string]any{
			"token_count": map[string]any{"totalTokens": 1117, "inputTokens": 100, "outputTokens": 7,
				"cachedInputTokens": 1000, "cachedWriteTokens": 10, "reasoningOutputTokens": 0},
			"model_usage": []any{
				map[string]any{"model": "claude-opus-5-5", "token_count": map[string]any{"totalTokens": 1117,
					"inputTokens": 100, "outputTokens": 7, "cachedInputTokens": 1000,
					"cachedWriteTokens": 10, "reasoningOutputTokens": 0}},
				map[string]any{"model": "claude-sonnet-5-5", "token_count": map[string]any{"totalTokens": 2234,
					"inputTokens": 200, "outputTokens": 14, "cachedInputTokens": 2000,
					"cachedWriteTokens": 20, "reasoningOutputTokens": 0}},
			}}},
	})
	_, usage := probe.finished("with-agents")
	if usage == nil {
		t.Fatal("usage is recorded")
	}
	if usage.Scope != core.UsageScopeTurn {
		t.Fatalf("scope = %v", usage.Scope)
	}
	if usage.TotalTokens != 3351 || usage.InputTokens != 300 || usage.OutputTokens != 21 {
		t.Fatalf("usage = %+v", usage)
	}
	if usage.CachedReadTokens == nil || *usage.CachedReadTokens != 3000 ||
		usage.CachedWriteTokens == nil || *usage.CachedWriteTokens != 30 {
		t.Fatalf("usage = %+v", usage)
	}
	models := usage.ProviderDetails.ModelUsage
	if models["claude-opus-5-5"].TotalTokens != 1117 || models["claude-sonnet-5-5"].TotalTokens != 2234 {
		t.Fatalf("models = %+v", models)
	}

	// A partial breakdown must retain the known main-loop remainder instead
	// of shrinking the total or assigning that remainder to the parent model.
	partial := probe.prompt("partial", "reply directly")
	probe.chunk("Done.")
	probe.result(partial, map[string]any{"stopReason": "end_turn",
		"usage": map[string]any{"totalTokens": 1117, "inputTokens": 100, "outputTokens": 7,
			"cachedReadTokens": 1000, "cachedWriteTokens": 10},
		"_meta": map[string]any{"quota": map[string]any{"model_usage": []any{map[string]any{"model": "claude-opus-5-5",
			"token_count": map[string]any{"totalTokens": 111, "inputTokens": 10, "outputTokens": 1,
				"cachedInputTokens": 100, "cachedWriteTokens": 0}}}}},
	})
	_, usage = probe.finished("partial")
	if usage.TotalTokens != 1117 {
		t.Fatalf("usage = %+v", usage)
	}
	if usage.ProviderDetails.ModelUsage["claude-opus-5-5"].TotalTokens != 111 {
		t.Fatalf("usage = %+v", usage.ProviderDetails)
	}

	// Older adapters and turns with no breakdown retain their flat usage,
	// and never inherit the preceding turn's native-agent rows.
	next := probe.prompt("without-agents", "reply directly")
	probe.chunk("Done.")
	probe.result(next, map[string]any{"stopReason": "end_turn",
		"usage": map[string]any{"totalTokens": 11, "inputTokens": 8, "outputTokens": 3},
		"_meta": map[string]any{"quota": map[string]any{"model_usage": []any{}}},
	})
	_, usage = probe.finished("without-agents")
	if usage.TotalTokens != 11 || usage.ProviderDetails != nil {
		t.Fatalf("usage = %+v", usage)
	}
	probe.close()
}

func TestMalformedClaudeModelAccountingReportsIncompleteUsage(t *testing.T) {
	probe := newClaudeProbe(t)
	// Missing counters, inconsistent totals, duplicate rows and overflow must
	// not silently become complete usage attributed to the selected model.
	valid := func() map[string]any {
		return map[string]any{"model": "claude-sonnet-5-5", "token_count": map[string]any{
			"totalTokens": 11, "inputTokens": 8, "outputTokens": 3,
			"cachedInputTokens": 0, "cachedWriteTokens": 0}}
	}
	inconsistent := valid()
	inconsistent["token_count"].(map[string]any)["totalTokens"] = 12
	overflow := valid()
	overflow["token_count"].(map[string]any)["inputTokens"] = uint64(18446744073709551615)
	for _, rows := range []any{
		[]any{map[string]any{"model": "claude-sonnet-5-5", "token_count": map[string]any{"totalTokens": 11}}},
		[]any{inconsistent},
		[]any{valid(), valid()},
		[]any{overflow},
	} {
		prompt := probe.prompt("bad-accounting", "reply directly")
		probe.chunk("Done.")
		probe.result(prompt, map[string]any{"stopReason": "end_turn",
			"usage": map[string]any{"totalTokens": 11, "inputTokens": 8, "outputTokens": 3},
			"_meta": map[string]any{"quota": map[string]any{"model_usage": rows}},
		})
		for warned := false; !warned; {
			switch event := probe.event().(type) {
			case core.RuntimeEventWarning:
				warned = strings.HasPrefix(event.Message, "Claude per-model usage")
			case core.RuntimeEventPromptFinished:
				t.Fatal("malformed accounting must report its failure")
			}
		}
		_, usage := probe.finished("bad-accounting")
		if usage.Scope != core.UsageScopeUnspecified || usage.TotalTokens != 11 || usage.ProviderDetails != nil {
			t.Fatalf("usage = %+v", usage)
		}
	}
	probe.close()
}

// Hard-won: 65a7390: Claude prompts stayed running because the result from their answer cycle did not end them.
func TestAClaudePromptEndsAtItsResponseWhileBackgroundWorkContinues(t *testing.T) {
	probe := newClaudeProbe(t)
	first := probe.prompt("prompt-1", "start a background agent")
	probe.chunk("Started it; I will report back.")
	probe.sdkResult(claudeSuccess("human"))
	probe.noFinish(100 * time.Millisecond)
	probe.result(first, map[string]any{"stopReason": "end_turn", "usage": map[string]any{
		"totalTokens": 1117, "inputTokens": 100, "outputTokens": 7,
		"cachedReadTokens": 1000, "cachedWriteTokens": 10,
	}})
	stopReason, usage := probe.finished("prompt-1")
	if stopReason != "EndTurn" {
		t.Fatalf("stop reason = %s", stopReason)
	}
	if usage == nil {
		t.Fatal("the response's usage is recorded")
	}
	if usage.OutputTokens != 7 || usage.TotalTokens != 1117 {
		t.Fatalf("usage = %+v", usage)
	}
	probe.noMessage(50 * time.Millisecond)
	second := probe.prompt("prompt-2", "what did it find?")
	probe.chunk("It found three files.")
	probe.sdkResult(claudeSuccess("human"))
	probe.noFinish(100 * time.Millisecond)
	probe.result(second, map[string]any{"stopReason": "max_tokens"})
	if stopReason, _ := probe.finished("prompt-2"); stopReason != "MaxTokens" {
		t.Fatalf("stop reason = %s", stopReason)
	}
	probe.close()
}

func TestTheAdapterReplyStillEndsAPromptThatNoResultEnded(t *testing.T) {
	probe := newClaudeProbe(t)
	first := probe.prompt("prompt-1", "/context")
	probe.chunk("Context: 12k tokens")
	probe.result(first, map[string]any{"stopReason": "end_turn"})
	if stopReason, _ := probe.finished("prompt-1"); stopReason != "EndTurn" {
		t.Fatalf("stop reason = %s", stopReason)
	}
	// A result the coordinator relays after the reply won the race is for a
	// prompt that already ended.
	probe.sdkResult(claudeSuccess("human"))
	probe.noFinish(100 * time.Millisecond)
	second := probe.prompt("prompt-2", "carry on")
	probe.chunk("ok")
	probe.result(second, map[string]any{"stopReason": "end_turn"})
	if stopReason, _ := probe.finished("prompt-2"); stopReason != "EndTurn" {
		t.Fatalf("stop reason = %s", stopReason)
	}
	probe.close()
}

func TestACancelInFlightLetsTheReplyEndThePrompt(t *testing.T) {
	probe := newClaudeProbe(t)
	prompt := probe.prompt("prompt-1", "go")
	probe.command(CommandRequestCancel{RequestID: "cancel-1"})
	if message := probe.message(); message["method"] != "session/cancel" {
		t.Fatalf("message = %v", message)
	}
	probe.sdkResult(claudeSuccess("human"))
	probe.noFinish(100 * time.Millisecond)
	probe.result(prompt, map[string]any{"stopReason": "cancelled"})
	if stopReason, _ := probe.finished("prompt-1"); stopReason != "Cancelled" {
		t.Fatalf("stop reason = %s", stopReason)
	}
	probe.close()
}

func TestBackgroundAndInterruptedCyclesDoNotEndThePrompt(t *testing.T) {
	probe := newClaudeProbe(t)
	prompt := probe.prompt("prompt-1", "go")
	probe.sdkResult(claudeSuccess("task-notification"))
	probe.sdkResult(claudeSteerInterruption())
	probe.noFinish(100 * time.Millisecond)
	probe.chunk("done")
	probe.sdkResult(claudeSuccess("human"))
	probe.noFinish(100 * time.Millisecond)
	probe.result(prompt, map[string]any{"stopReason": "end_turn"})
	if stopReason, _ := probe.finished("prompt-1"); stopReason != "EndTurn" {
		t.Fatalf("stop reason = %s", stopReason)
	}
	probe.close()
}

func claudeSteer(probe *claudeProbe) map[string]any {
	probe.t.Helper()
	probe.command(CommandRequestSteer{
		RequestID:      "steer-1",
		ActivePromptID: "prompt-1",
		SteeringPrompt: core.ClaimedSteeringPrompt{
			QueuedCommandID: "queued-1",
			Prompt:          textPrompt("use the other file"),
		},
	})
	steering := probe.message()
	if steering["method"] != sessionSteeringMethod {
		probe.t.Fatalf("steering = %v", steering)
	}
	return steering
}

func claudeSteerApplied(probe *claudeProbe) {
	probe.t.Helper()
	for {
		switch event := probe.event().(type) {
		case core.RuntimeEventSteerApplied:
			if event.RequestID != "steer-1" {
				probe.t.Fatalf("steer applied = %s", event.RequestID)
			}
			return
		case core.RuntimeEventPromptFinished:
			probe.t.Fatal("the prompt finished before the steer was applied")
		}
	}
}

// Hard-won: 65a7390: A steered Claude cycle could finish without settling the prompt when no interruption flag arrived.
func TestTheSteeredCycleEndsThePromptWhetherOrNotAnInterruptionIsReported(t *testing.T) {
	for _, interrupted := range []bool{true, false} {
		probe := newClaudeProbe(t)
		prompt := probe.prompt("prompt-1", "go")
		probe.chunk("Working on the first file")
		steering := claudeSteer(probe)
		probe.result(steering, map[string]any{"outcome": "injected"})
		claudeSteerApplied(probe)
		if interrupted {
			probe.sdkResult(claudeSteerInterruption())
		}
		probe.noFinish(100 * time.Millisecond)
		probe.chunk("Switched to the other file.")
		probe.sdkResult(claudeSuccess("human"))
		probe.noFinish(100 * time.Millisecond)
		probe.result(prompt, map[string]any{"stopReason": "end_turn"})
		if stopReason, _ := probe.finished("prompt-1"); stopReason != "EndTurn" {
			t.Fatalf("%v: stop reason = %s", interrupted, stopReason)
		}
		probe.close()
	}
}

func TestASteerStillWaitingForItsAcknowledgementIsSettledBeforeThePromptEnds(t *testing.T) {
	probe := newClaudeProbe(t)
	prompt := probe.prompt("prompt-1", "go")
	steering := claudeSteer(probe)
	probe.chunk("done")
	probe.sdkResult(claudeSuccess("human"))
	probe.result(prompt, map[string]any{"stopReason": "end_turn"})
	// The loop waits briefly for the steer's answer before ending the prompt.
	probe.result(steering, map[string]any{"outcome": "injected"})
	claudeSteerApplied(probe)
	if stopReason, _ := probe.finished("prompt-1"); stopReason != "EndTurn" {
		t.Fatalf("stop reason = %s", stopReason)
	}
	probe.close()
}

func TestAutonomousTurnSpawnsAreLiveEdgesAndNotReplayedHistory(t *testing.T) {
	probe := claudeProbeWithReplayedAutonomousTurn(t, true)
	probe.autonomousTurn("live-turn")
	replaySeen := false
	for {
		changed, ok := probe.event().(core.RuntimeEventAirTaskChanged)
		if !ok {
			continue
		}
		spawned, ok := changed.Update.(core.AirTaskUpdateTurnSpawned)
		if !ok {
			continue
		}
		if changed.SessionID != "claude-session" {
			t.Fatalf("session = %s", changed.SessionID)
		}
		if spawned.TaskID == "replayed-turn" {
			if !spawned.Replayed {
				t.Fatal("the turn before the load response is replayed history")
			}
			replaySeen = true
			continue
		}
		if spawned.TaskID != "live-turn" || spawned.Replayed || !replaySeen {
			t.Fatalf("spawned = %+v, replay seen = %v", spawned, replaySeen)
		}
		break
	}
	probe.close()
}

// Stop while Claude Code works on its own reaches the adapter as
// `session/cancel`. The relay ends that turn at the interrupted cycle's
// result, which the runtime forwards like any other.
// Hard-won: 4c379ac: Escape did nothing while Claude continued its own background work.
func TestStopWhileClaudeWorksOnItsOwnSendsSessionCancel(t *testing.T) {
	probe := newClaudeProbe(t)
	probe.autonomousTurn("silent-turn")
	probe.command(CommandRequestCancel{RequestID: "stop-1"})
	if message := probe.message(); message["method"] != "session/cancel" {
		t.Fatalf("message = %v", message)
	}
	for applied := false; !applied; {
		switch event := probe.event().(type) {
		case core.RuntimeEventCancelApplied:
			if event.RequestID != "stop-1" {
				t.Fatalf("cancel applied = %s", event.RequestID)
			}
			applied = true
		case core.RuntimeEventCommandRejected:
			t.Fatalf("Stop was refused: %s", event.Message)
		}
	}
	interrupted := claudeSuccess("task-notification")
	interrupted["subtype"] = "error_during_execution"
	interrupted["is_error"] = true
	probe.sdkResult(interrupted)
	for {
		if result, ok := probe.event().(core.RuntimeEventClaudeTurnResult); ok {
			if result.OriginKind == nil || *result.OriginKind != "task-notification" {
				t.Fatalf("origin = %v", result.OriginKind)
			}
			break
		}
	}
	probe.close()
}

// A local command such as `/context` makes no model call. The adapter sends
// its text after the result and then answers the prompt, so the reply ends
// the prompt and the text is recorded inside it.
func TestALocalCommandEndsAtTheReplyAfterItsText(t *testing.T) {
	probe := newClaudeProbe(t)
	prompt := probe.prompt("prompt-1", "/context")
	local := claudeSuccess("human")
	local["num_turns"] = 0
	local["usage"].(map[string]any)["output_tokens"] = 0
	local["local_command"] = "/context"
	local["result"] = "Context: 12k of 200k tokens"
	probe.sdkResult(local)
	probe.chunk("Context: 12k of 200k tokens")
	probe.result(prompt, map[string]any{"stopReason": "end_turn"})
	textSeen := false
	for {
		switch event := probe.event().(type) {
		case core.RuntimeEventSessionUpdate:
			if jsonAt(decodeJSONValue(t, event.Update), "sessionUpdate") == "agent_message_chunk" {
				textSeen = true
			}
			continue
		case core.RuntimeEventPromptFinished:
			if event.RequestID != "prompt-1" || event.StopReason != "EndTurn" {
				t.Fatalf("finished = %+v", event)
			}
			if !textSeen {
				t.Fatal("the command's text belongs to its prompt")
			}
		default:
			continue
		}
		break
	}
	probe.close()
}

// R4-3: a session that only ever changed its model made the adapter send
// "Auto mode unavailable" as agent text. Claude Code wrote no transcript,
// because no model cycle ran, yet that text marked the session used, and
// the worker that replaced it refused to start a fresh session. Only a
// cycle's result, or a prompt, is evidence that Claude Code wrote one.
// Hard-won: f70600d: An adapter notice marked a never-prompted Claude session used and caused resume crash loops.
func TestAdapterTextOutsideAnyCycleDoesNotMarkTheSessionUsed(t *testing.T) {
	probe := newClaudeProbe(t)
	probe.chunk("**Auto mode unavailable:** the selected model does not support Auto mode; using Accept edits instead.")
	for done := false; !done; {
		switch event := probe.event().(type) {
		case core.RuntimeEventNativeSessionUsed:
			t.Fatal("adapter text with no cycle marked the native session used")
		case core.RuntimeEventSessionUpdate:
			done = jsonAt(decodeJSONValue(t, event.Update), "sessionUpdate") == "agent_message_chunk"
		}
	}

	// A result is a cycle Claude Code ran, and wrote down.
	probe.sdkResult(claudeSuccess("task-notification"))
	used := false
	for done := false; !done; {
		switch probe.event().(type) {
		case core.RuntimeEventNativeSessionUsed:
			used = true
		case core.RuntimeEventClaudeTurnResult:
			done = true
		}
	}
	if !used {
		t.Fatal("the result must be reported before it is relayed")
	}
	probe.close()
}

// Port of mj-worker/src/acp/grok_usage_tests.rs.

package worker

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// spec is the launch of a Grok bridge at command; the session directory
// tests share it.
func spec(command string, environment map[string]string, cwd string) LaunchSpec {
	if environment == nil {
		environment = map[string]string{}
	}
	return LaunchSpec{
		OperatorAway:         &OperatorAway{},
		GoalRecovery:         NewShared(core.GoalRecoveryContext{}),
		SubagentPolicy:       core.SubagentPolicy{Mode: core.SubagentPolicyNative},
		Command:              command,
		Args:                 []string{},
		Environment:          environment,
		Cwd:                  cwd,
		NativeSessionHistory: NativeSessionHistoryLoadBeforeReplace,
		AcceptedConfig:       NewShared(core.AcceptedSessionConfig{}),
		Harness:              core.HarnessKindGrok,
		ExecutionPolicy:      core.ExecutionPolicyConfiguredApprovals,
		ApprovalChoices:      core.DefaultApprovalChoices(),
		ACPActivity:          core.NewAcpActivityClock(),
		StepClock:            &core.StepClock{},
		ToolsInFlight:        &core.ToolsInFlight{},
		TurnContext:          &transcript.TurnContext{},
		LoginIdentity:        &ClaudeLoginIdentity{},
		Verdict:              VerdictSourceDisabled{},
	}
}

func grokUsageTestsReport() map[string]any {
	return map[string]any{
		"inputTokens": 100, "outputTokens": 30, "totalTokens": 130, "cachedReadTokens": 40,
		"cacheCreationTokens": 10, "reasoningTokens": 20, "modelCalls": 2, "apiDurationMs": 1700, "costUsdTicks": 88767200,
		"modelUsage": map[string]any{"grok-4.6-build": map[string]any{"inputTokens": 100, "outputTokens": 30, "costUsdTicks": 88767200}},
	}
}

func grokUsageTestsWrite(t *testing.T, writer io.Writer, message any) error {
	encoded, err := json.Marshal(message)
	if err != nil {
		t.Errorf("encode %v: %v", message, err)
		return err
	}
	_, err = writer.Write(append(encoded, '\n'))
	return err
}

func TestGrokUsageCrossesACPWithMetadataDuplicatesAndLateNotifications(t *testing.T) {
	client, bridgeEnd := net.Pipe()
	bridge := make(chan int, 1)
	go func() {
		defer bridgeEnd.Close()
		lines := bufio.NewReader(bridgeEnd)
		prompts := 0
		defer func() { bridge <- prompts }()
		for {
			line, err := lines.ReadBytes('\n')
			if err != nil {
				return
			}
			var request map[string]any
			if err := json.Unmarshal(line, &request); err != nil {
				t.Errorf("request %s: %v", line, err)
				return
			}
			id := request["id"]
			method, _ := request["method"].(string)
			var result any
			switch method {
			case "initialize":
				result = map[string]any{"protocolVersion": 1}
			case "session/new":
				result = map[string]any{"sessionId": "grok-test"}
			case "session/prompt":
				prompts++
				nativeID := fmt.Sprintf("native-%d", prompts)
				notification := map[string]any{"jsonrpc": "2.0", "method": "_x.ai/session/update", "params": map[string]any{
					"sessionId": "grok-test",
					"update": map[string]any{"sessionUpdate": "turn_completed", "prompt_id": nativeID,
						"usage": grokUsageTestsReport(), "elapsed_ms": 2000},
				}}
				if prompts == 1 {
					chunk := map[string]any{"jsonrpc": "2.0", "method": "session/update", "params": map[string]any{
						"sessionId": "grok-test",
						"update": map[string]any{"sessionUpdate": "agent_message_chunk",
							"content": map[string]any{"type": "text", "text": strings.Repeat("x", 80*1024)}},
					}}
					if grokUsageTestsWrite(t, bridgeEnd, chunk) != nil {
						return
					}
				}
				if prompts == 2 {
					for range 2 {
						if grokUsageTestsWrite(t, bridgeEnd, notification) != nil {
							return
						}
					}
				}
				if prompts == 3 {
					response := map[string]any{"jsonrpc": "2.0", "id": id, "result": map[string]any{
						"stopReason": "end_turn", "_meta": map[string]any{"promptId": nativeID}}}
					if grokUsageTestsWrite(t, bridgeEnd, response) != nil {
						return
					}
					time.Sleep(10 * time.Millisecond)
					if grokUsageTestsWrite(t, bridgeEnd, notification) != nil {
						return
					}
					continue
				}
				result = map[string]any{
					"stopReason": "end_turn",
					"usage":      map[string]any{"inputTokens": 100, "outputTokens": 30, "totalTokens": 130},
					"_meta":      map[string]any{"promptId": nativeID, "usage": grokUsageTestsReport()},
				}
			default:
				continue
			}
			if grokUsageTestsWrite(t, bridgeEnd, map[string]any{"jsonrpc": "2.0", "id": id, "result": result}) != nil {
				return
			}
		}
	}()
	transport := NewByteStreams(client, client)
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 16)
	cwd, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	driver := make(chan error, 1)
	go func() {
		_, err := drive(context.Background(), transport, spec("fake", map[string]string{}, cwd), requests, events,
			NewShared[*OpenedSession](nil), false)
		driver <- err
	}()
	for index := 1; index <= 3; index++ {
		requests <- CommandRequestPrompt{
			RequestID: fmt.Sprintf("prompt-%d", index),
			Prompt:    []schema.ContentBlock{acpFeaturesText("test")},
		}
	prompt:
		for {
			var event core.RuntimeEvent
			select {
			case event = <-events:
			case <-time.After(10 * time.Second):
				t.Fatal("the bridge must make progress")
			}
			switch event := event.(type) {
			case core.RuntimeEventWarning:
				if strings.Contains(event.Message, "usage report") {
					t.Fatal(event.Message)
				}
			case core.RuntimeEventPromptFinished:
				if event.StopReason != "EndTurn" {
					t.Fatalf("stop reason %s", event.StopReason)
				}
				usage := event.Usage
				if usage == nil {
					t.Fatal("Grok full-turn usage must survive the actual ACP connection")
				}
				if usage.Scope != core.UsageScopeTurn || usage.TotalTokens != 130 {
					t.Fatalf("usage %+v", usage)
				}
				if usage.CachedReadTokens == nil || *usage.CachedReadTokens != 40 {
					t.Fatalf("cached read %v", usage.CachedReadTokens)
				}
				details := usage.ProviderDetails
				if details.Cost == nil || details.Cost.Usd != "0.0088767200" {
					t.Fatalf("cost %+v", details.Cost)
				}
				if index > 1 && (details.ElapsedMs == nil || *details.ElapsedMs != 2000) {
					t.Fatalf("elapsed %v", details.ElapsedMs)
				}
				break prompt
			}
		}
	}
	close(requests)
	select {
	case err := <-driver:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(10 * time.Second):
		t.Fatal("the driver did not stop")
	}
	if prompts := <-bridge; prompts != 3 {
		t.Fatalf("%d prompts", prompts)
	}
}

// Requires GROK_VALIDATION_PROGRAM and GROK_VALIDATION_AUTH_FILE; runs one
// minimal live turn (Rust #[ignore]).
func TestGrokLiveUsageReachesRuntimeCompletion(t *testing.T) {
	program, ok := os.LookupEnv("GROK_VALIDATION_PROGRAM")
	if !ok {
		t.Skip("requires GROK_VALIDATION_PROGRAM and GROK_VALIDATION_AUTH_FILE; runs one minimal live turn")
	}
	auth, ok := os.LookupEnv("GROK_VALIDATION_AUTH_FILE")
	if !ok {
		t.Fatal("explicit authentication source")
	}
	parent, err := filepath.Abs("../../../target/grok-acp-validation")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(parent, 0o755); err != nil {
		t.Fatal(err)
	}
	temp, err := os.MkdirTemp(parent, "")
	if err != nil {
		t.Fatal(err)
	}
	defer os.RemoveAll(temp)
	home := filepath.Join(temp, "home")
	cwd := filepath.Join(temp, "workspace")
	for _, dir := range []string{home, cwd} {
		if err := os.Mkdir(dir, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	login, err := os.ReadFile(auth)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(home, "auth.json"), login, 0o600); err != nil {
		t.Fatal(err)
	}
	launch := spec(program, map[string]string{"GROK_HOME": home}, cwd)
	launch.Args = []string{"agent", "--no-leader", "stdio"}
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	shutdown, stop := context.WithCancel(context.Background())
	driver := make(chan error, 1)
	go func() { driver <- RunWithShutdown(shutdown, launch, requests, events) }()
	requests <- CommandRequestPrompt{
		RequestID: "live-usage",
		Prompt:    []schema.ContentBlock{acpFeaturesText("Reply with exactly OK. Do not use tools.")},
	}
	var finished *core.RuntimeEventPromptFinished
	deadline := time.After(120 * time.Second)
live:
	for {
		select {
		case event := <-events:
			switch event := event.(type) {
			case core.RuntimeEventPromptFinished:
				finished = &event
				break live
			case core.RuntimeEventWarning:
				t.Log(event.Message)
			case core.RuntimeEventStopped:
				t.Fatal("runtime stopped before the live prompt completed")
			}
		case <-deadline:
			break live
		}
	}
	stop()
	// Keep draining while cleanup sends its final events.
	go func() {
		for range events {
		}
	}()
	select {
	case err := <-driver:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(30 * time.Second):
		t.Fatal("the runtime did not stop")
	}
	if finished == nil {
		t.Fatal("the live prompt did not complete")
	}
	if finished.StopReason != "EndTurn" {
		t.Fatalf("stop reason %s", finished.StopReason)
	}
	usage := finished.Usage
	if usage == nil {
		t.Fatal("live Grok response must include consumption")
	}
	if usage.Scope != core.UsageScopeTurn || usage.TotalTokens != usage.InputTokens+usage.OutputTokens || usage.TotalTokens == 0 {
		t.Fatalf("usage %+v", usage)
	}
	if len(usage.ProviderDetails.ModelUsage) == 0 {
		t.Fatal("no model usage")
	}
	encoded, _ := json.Marshal(usage)
	t.Logf("live Grok normalized usage: %s", encoded)
}

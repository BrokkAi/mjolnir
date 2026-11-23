//go:build unix

// Port of mj-worker/src/acp/muse_tests.rs.

package worker

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// museRuntime runs the ACP runtime as Rust's `spawn(run(spec, receiver,
// sender))` does and returns its command and event channels and its result.
func museRuntime(spec LaunchSpec) (chan CommandRequest, chan core.RuntimeEvent, <-chan error) {
	commands := make(chan CommandRequest, 16)
	events := make(chan core.RuntimeEvent, 128)
	result := make(chan error, 1)
	go func() { result <- Run(spec, commands, events) }()
	return commands, events, result
}

// awaitRuntime waits for the runtime's result; Rust's
// `timeout(.., task).await.unwrap().unwrap()`.
func awaitRuntime(t *testing.T, result <-chan error, wait time.Duration, events <-chan core.RuntimeEvent) error {
	t.Helper()
	timer := time.NewTimer(wait)
	defer timer.Stop()
	for {
		select {
		case err := <-result:
			return err
		case <-events:
			// The runtime's final reports need a reader, as Rust's dropped
			// receiver needs none.
		case <-timer.C:
			t.Fatal("the runtime must stop")
		}
	}
}

// nativeMuseTurn exercises the actual client and native host without
// printing credentials or provider transcripts in diagnostics. It runs one
// prompt through the Muse adapter at adapter, resuming the native session
// resume when it is not nil, and returns the native session id and the
// reply.
func nativeMuseTurn(t *testing.T, adapter, muse, home, cwd string, resume *string, prompt string) (string, string) {
	t.Helper()
	environment := map[string]string{
		"MUSE_CLI":        muse,
		"MUSE_SERVE_ARGS": "--disable-shell --disable-write",
	}
	core.HarnessKindMuse.ConfigureHomeEnvironment(home, environment)
	resuming := resume != nil
	spec := acpDriveSpec(adapter, core.HarnessKindMuse, core.ExecutionPolicyConfiguredApprovals, cwd)
	spec.Environment = environment
	spec.ResumeSession = resume
	commands, events, result := museRuntime(spec)
	sessionID := ""
	var reply strings.Builder
	var stopReason string
	for finished := false; !finished; {
		var event core.RuntimeEvent
		select {
		case event = <-events:
		case <-time.After(120 * time.Second):
			t.Fatal("native Muse timed out")
		}
		switch event := event.(type) {
		case core.RuntimeEventSessionStarted:
			if event.Resumed != resuming {
				t.Fatalf("resumed = %v", event.Resumed)
			}
			fmt.Fprintf(os.Stderr, "Native Muse session ready (resumed=%v)\n", event.Resumed)
			sessionID = event.NativeSessionID
			commands <- CommandRequestPrompt{RequestID: "native-smoke", Prompt: textPrompt(prompt)}
		case core.RuntimeEventSessionUpdate:
			update := decodeJSONValue(t, event.Update)
			if jsonAt(update, "sessionUpdate") == "agent_message_chunk" {
				if text, ok := jsonAt(update, "content", "text").(string); ok {
					reply.WriteString(text)
				}
			}
		case core.RuntimeEventPromptFinished:
			stopReason = event.StopReason
			finished = true
		case core.RuntimeEventElicitationRequested:
			resolved := make(chan error, 1)
			commands <- CommandRequestResolveElicitation{
				ElicitationID: event.Request.ID,
				Response:      core.ElicitationResponse{Action: core.ElicitationResponseCancel},
				Resolved:      resolved,
			}
			if err := <-resolved; err != nil {
				t.Fatal(err)
			}
		case core.RuntimeEventWarning:
			if strings.Contains(event.Message, "ACP runtime failed") {
				t.Fatal("native Muse ACP runtime failed; check authentication and runtime configuration")
			}
		}
	}
	commands <- CommandRequestClose{RequestID: "native-close"}
	close(commands)
	if err := awaitRuntime(t, result, 15*time.Second, events); err != nil {
		t.Fatal(err)
	}
	if sessionID == "" {
		t.Fatal("the session opened")
	}
	if stopReason != "EndTurn" {
		t.Fatalf("native Muse turn failed: %s", stopReason)
	}
	return sessionID, reply.String()
}

func museNext(t *testing.T, events <-chan core.RuntimeEvent) core.RuntimeEvent {
	t.Helper()
	select {
	case event := <-events:
		return event
	case <-time.After(15 * time.Second):
		t.Fatal("Muse event timed out")
	}
	return nil
}

// museOpened is what opening a session through the fake muse-acp produced.
type museOpened struct {
	outcome    error
	configured bool
	warnings   []string
	// received is the MCP server names each `session/new` carried.
	received [][]string
	standing string
}

const fakeMuseACP = `
import json, os, sys
host, log = os.environ['MJ_FAKE_MUSE_HOST'], os.environ['MJ_FAKE_MUSE_LOG']
options = json.loads(os.environ['MJ_FAKE_MUSE_OPTIONS'])
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get('method'), request.get('id')
    if ident is None: continue
    params = request.get('params', {})
    if method == 'initialize':
        # muse-acp advertises HTTP MCP exactly when its host granted sessionMcp.
        reply = {'result': {'protocolVersion': 1, 'agentCapabilities': {
            'mcpCapabilities': {'http': host == 'granted'}, 'sessionCapabilities': {'systemPrompt': {}}}}}
    elif method == 'session/new':
        with open(log + '.prompt', 'w') as prompt:
            prompt.write(json.dumps(params.get('systemPrompt')))
        with open(log, 'a') as out:
            out.write(json.dumps([s['name'] for s in params.get('mcpServers', [])]) + '\n')
        if host == 'failed':
            reply = {'error': {'code': -32000, 'message': 'Muse Code is not logged in'}}
        else:
            reply = {'result': {'sessionId': 'native', 'configOptions': options}}
    elif method == 'session/set_config_option':
        for option in options:
            if option['id'] == params['configId']:
                option['currentValue'] = params['value']
        reply = {'result': {'configOptions': options}}
    else:
        reply = {'result': {}}
    print(json.dumps({'jsonrpc': '2.0', 'id': ident, **reply}), flush=True)
`

// openMuse opens a session through a fake muse-acp whose host grants
// session MCP, withholds it, or could not start. A session carries project
// memory.
func openMuse(t *testing.T, host string) museOpened {
	t.Helper()
	root := t.TempDir()
	script := filepath.Join(root, "muse_acp.py")
	log := filepath.Join(root, "mcp.jsonl")
	if err := os.WriteFile(script, []byte(fakeMuseACP), 0o644); err != nil {
		t.Fatal(err)
	}
	options, err := json.Marshal(musePolicyOptions())
	if err != nil {
		t.Fatal(err)
	}
	spec := acpDriveSpec("python3", core.HarnessKindMuse, core.ExecutionPolicyConfiguredApprovals, root)
	spec.SubagentMCPSocket = &SubagentMcpSocket{
		Path:                 filepath.Join(root, "subagents.sock"),
		Role:                 core.SubagentMcpRoleMessageOnly,
		Oracle:               false,
		OrchestratorsEnabled: false,
		SubtleSubagents:      false,
		ReviewTools:          false,
		ProfileRegistration:  false,
	}
	spec.Args = []string{script}
	spec.Environment = map[string]string{
		"MJ_FAKE_MUSE_HOST":    host,
		"MJ_FAKE_MUSE_OPTIONS": string(options),
		"MJ_FAKE_MUSE_LOG":     log,
	}
	historySocket := filepath.Join(root, "history.sock")
	spec.ProjectMemory = &core.ProjectMemoryLaunchConfig{
		HistorySocket:   &historySocket,
		ProjectKey:      "abc",
		Root:            filepath.Join(root, "memory"),
		BaselineRoot:    filepath.Join(root, ".hel-memory-baseline"),
		RepositoryRoots: map[string]string{},
		MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
	}
	commands, events, result := museRuntime(spec)
	configured := false
	var warnings []string
	var outcome error
	stopped := false
	for !configured && !stopped {
		select {
		case event := <-events:
			switch event := event.(type) {
			case core.RuntimeEventSessionConfigured:
				configured = true
			case core.RuntimeEventWarning:
				warnings = append(warnings, event.Message)
			}
		case outcome = <-result:
			stopped = true
		case <-time.After(15 * time.Second):
			t.Fatal("the fake adapter must make progress")
		}
	}
	close(commands)
	if !stopped {
		outcome = awaitRuntime(t, result, 15*time.Second, events)
	}
	var received [][]string
	if data, err := os.ReadFile(log); err == nil {
		for _, line := range strings.Split(strings.TrimRight(string(data), "\n"), "\n") {
			if line == "" {
				continue
			}
			var names []string
			if err := json.Unmarshal([]byte(line), &names); err != nil {
				t.Fatal(err)
			}
			received = append(received, names)
		}
	}
	standing, _ := os.ReadFile(log + ".prompt")
	return museOpened{
		outcome:    outcome,
		configured: configured,
		warnings:   warnings,
		received:   received,
		standing:   string(standing),
	}
}

func containsString(list []string, value string) bool {
	for _, item := range list {
		if item == value {
			return true
		}
	}
	return false
}

// Hard-won: c5deb2a: Older Muse containers stopped working after upgrades when their adapter withheld MCP servers.
// Hard-won: 46a1cb7a5: standing guidance advertised tools withheld by the adapter.
func TestMuseSessionsReceiveMcpServersAndSaySoWhenTheHostWithholdsThem(t *testing.T) {
	opened := openMuse(t, "granted")
	if opened.outcome != nil {
		t.Fatal(opened.outcome)
	}
	if !opened.configured || len(opened.warnings) != 0 {
		t.Fatalf("configured = %v, warnings = %#v", opened.configured, opened.warnings)
	}
	if !containsString(opened.received[0], "mj-memory") || !containsString(opened.received[0], "mj-agents") {
		t.Fatalf("received = %v", opened.received)
	}
	if !strings.Contains(opened.standing, "search_sessions") || !strings.Contains(opened.standing, "send_message") {
		t.Fatal(opened.standing)
	}

	// A container keeps the adapter it was created with. Its session keeps
	// working without the tools, and the person is told why.
	opened = openMuse(t, "withheld")
	if opened.outcome != nil {
		t.Fatal(opened.outcome)
	}
	if !opened.configured {
		t.Fatal("the session is configured")
	}
	if !containsString(opened.received[0], "mj-memory") || !containsString(opened.received[0], "mj-agents") {
		t.Fatalf("received = %v", opened.received)
	}
	if strings.Contains(opened.standing, "search_sessions") || strings.Contains(opened.standing, "send_message") {
		t.Fatal(opened.standing)
	}
	warned := false
	for _, warning := range opened.warnings {
		if strings.Contains(warning, "does not accept MCP servers") {
			warned = true
		}
	}
	if !warned {
		t.Fatalf("warnings = %#v", opened.warnings)
	}
}

// Hard-won: c5deb2a: A Muse reviewer could run without the analyzer tools required to review safely.
func TestAMuseHostThatCouldNotStartReportsItsOwnDiagnostic(t *testing.T) {
	// A host that never started also withholds the grant; its diagnostic, not
	// the missing grant, is what the person has to act on.
	opened := openMuse(t, "failed")
	if opened.outcome == nil {
		t.Fatal("the session fails")
	}
	reason := WorkerExitReason(opened.outcome)
	if !strings.Contains(reason, fmt.Sprintf("create ACP session (%s): Muse Code is not logged in", core.PromptAuthRequiredMarker)) {
		t.Fatal(reason)
	}
	if strings.Contains(reason, "MCP") {
		t.Fatal(reason)
	}
	if opened.configured {
		t.Fatal("the session never configured")
	}
}

func TestRealMuseAdapterChatSelectorsImagesPermissionsQuestionsAndResume(t *testing.T) {
	acpDriveSkipUnlessIgnoredTestsRequested(t, "requires MJ_MUSE_ACP_TEST_BINARY pointing to verified muse-acp 0.25.0")
	adapter, ok := os.LookupEnv("MJ_MUSE_ACP_TEST_BINARY")
	if !ok {
		t.Fatal("set adapter path")
	}
	host, err := filepath.Abs(filepath.Join("..", "..", "..", "tests", "e2e", "muse_host.py"))
	if err != nil {
		t.Fatal(err)
	}
	for _, scenario := range []struct {
		name   string
		choice string
		resume bool
		policy core.ExecutionPolicy
	}{
		{"chat", "", false, core.ExecutionPolicyConfiguredApprovals},
		{"chat", "", true, core.ExecutionPolicyConfiguredApprovals},
		{"chat", "", false, core.ExecutionPolicyUnconstrained},
		{"permission", "allow", false, core.ExecutionPolicyConfiguredApprovals},
		{"permission", "deny", false, core.ExecutionPolicyConfiguredApprovals},
		{"question", "accept", false, core.ExecutionPolicyConfiguredApprovals},
		{"question", "cancel", false, core.ExecutionPolicyConfiguredApprovals},
		{"quiet", "", false, core.ExecutionPolicyConfiguredApprovals},
	} {
		fmt.Fprintf(os.Stderr, "Muse scenario: %s, choice: %s, resume: %v, policy: %s\n",
			scenario.name, scenario.choice, scenario.resume, scenario.policy)
		temp := t.TempDir()
		log := filepath.Join(temp, "host.jsonl")
		review := "allow"
		if scenario.choice == "deny" {
			review = "deny"
		}
		spec := acpDriveSpec(adapter, core.HarnessKindMuse, scenario.policy, temp)
		spec.Environment = map[string]string{
			"MUSE_CLI":              host,
			"MJ_MUSE_TEST_LOG":      log,
			"MJ_MUSE_TEST_SCENARIO": scenario.name,
			// The verdict the fake auto-review host returns.
			"MJ_MUSE_TEST_REVIEW": review,
		}
		if scenario.resume {
			session := "01991be0-0000-7000-8000-000000000001"
			spec.ResumeSession = &session
		}
		commands, events, result := museRuntime(spec)
		for started := false; !started; {
			switch event := museNext(t, events).(type) {
			case core.RuntimeEventSessionStarted:
				if event.Resumed != scenario.resume {
					t.Fatalf("resumed = %v", event.Resumed)
				}
				started = true
			case core.RuntimeEventSessionUpdate:
				if strings.Contains(string(event.Update), "must not replay") {
					t.Fatal(string(event.Update))
				}
			case core.RuntimeEventWarning:
				t.Fatalf("Muse startup failed: %s", event.Message)
			}
		}
		commands <- CommandRequestSetConfig{RequestID: "effort", Key: "reasoning_effort", Value: "high"}
		for {
			if applied, ok := museNext(t, events).(core.RuntimeEventConfigApplied); ok {
				if applied.Key != "reasoning_effort" || applied.Value != "high" {
					t.Fatalf("applied = %+v", applied)
				}
				break
			}
		}
		commands <- CommandRequestPrompt{RequestID: "prompt", Prompt: []schema.ContentBlock{
			{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: "test Muse"}},
			{Kind: schema.ContentBlockKindImage, Image: &schema.ImageContent{Data: strings.Repeat("QUJD", 32768), MimeType: "image/png"}},
		}}
		if scenario.name == "quiet" {
			// Wait for host admission, so cancellation tests an active turn.
			for range 200 {
				if data, _ := os.ReadFile(log); strings.Contains(string(data), "turn/start") {
					break
				}
				time.Sleep(10 * time.Millisecond)
			}
			commands <- CommandRequestCancel{RequestID: "cancel"}
		}
		forms := 0
		replied := false
		for finished := false; !finished; {
			switch event := museNext(t, events).(type) {
			case core.RuntimeEventSessionUpdate:
				if strings.Contains(string(event.Update), "must not replay") {
					t.Fatal(string(event.Update))
				}
				replied = replied || strings.Contains(string(event.Update), "Muse test reply")
			case core.RuntimeEventElicitationRequested:
				request := event.Request
				// Guardian answers Muse permissions with auto-review.
				if scenario.name != "question" {
					t.Fatalf("%+v", request)
				}
				// One form per question, titled by its header, with "Explain
				// instead" offered inside it: no route form first, and no
				// field shown by its id.
				if request.Message != "Which implementation?" {
					t.Fatalf("%+v", request)
				}
				var field *core.ElicitationField
				for i := range request.Fields {
					if _, ok := request.Fields[i].Kind.(core.ElicitationFieldKindSingleSelect); ok {
						field = &request.Fields[i]
						break
					}
				}
				if field == nil {
					t.Fatal("the question is a single select")
				}
				if field.Title != "Choose" {
					t.Fatalf("%+v", request)
				}
				options := field.Kind.(core.ElicitationFieldKindSingleSelect).Options
				explain := false
				for _, option := range options {
					explain = explain || option.Title == "Explain instead"
				}
				if !explain {
					t.Fatalf("%+v", request)
				}
				// The host's option description and preview reach the form.
				if options[0].Description == nil || *options[0].Description != "Keep the current design" ||
					options[0].Preview == nil || *options[0].Preview != "fn first() {}" {
					t.Fatalf("first option = %+v", options[0])
				}
				response := core.ElicitationResponse{Action: core.ElicitationResponseCancel}
				if scenario.choice == "accept" {
					response = core.ElicitationResponse{Action: core.ElicitationResponseAccept,
						Content: map[string]core.ElicitationValue{field.ID: core.ElicitationValueString("First")}}
				}
				resolved := make(chan error, 1)
				commands <- CommandRequestResolveElicitation{ElicitationID: request.ID, Response: response, Resolved: resolved}
				if err := <-resolved; err != nil {
					t.Fatal(err)
				}
				forms++
			case core.RuntimeEventPromptFinished:
				if event.StopReason == "error" {
					t.Fatalf("scenario %s", scenario.name)
				}
				finished = true
			case core.RuntimeEventWarning:
				if strings.Contains(event.Message, "ACP runtime failed") {
					t.Fatal(event.Message)
				}
			}
		}
		if scenario.name == "chat" && !replied {
			t.Fatal("the chat scenario replies")
		}
		if scenario.name == "question" && forms != 1 {
			t.Fatalf("scenario %s must show exactly one form", scenario.name)
		}
		commands <- CommandRequestClose{RequestID: "close"}
		close(commands)
		if err := awaitRuntime(t, result, 15*time.Second, events); err != nil {
			t.Fatal(err)
		}
		data, err := os.ReadFile(log)
		if err != nil {
			t.Fatal(err)
		}
		trace := string(data)
		if !strings.Contains(trace, "QUJDQUJD") {
			t.Fatal("the image reaches the host")
		}
		var messages []map[string]any
		for _, line := range strings.Split(strings.TrimRight(trace, "\n"), "\n") {
			var message map[string]any
			if err := json.Unmarshal([]byte(line), &message); err != nil {
				t.Fatal(err)
			}
			messages = append(messages, message)
		}
		// muse-acp 0.10 keeps the approval policy on `approval_mode`, apart
		// from its session modes. Guardian sessions review
		// (`promptUnmatched`); unconstrained sessions run `allowAll` without
		// permission checks, so the adapter's auto-review would never fire.
		var approvalModes []any
		for _, message := range messages {
			if message["method"] == "session/setApprovalMode" {
				approvalModes = append(approvalModes, jsonAt(message, "params", "mode"))
			}
		}
		expected := "promptUnmatched"
		if scenario.policy == core.ExecutionPolicyUnconstrained {
			expected = "allowAll"
		}
		if !jsonEqual(approvalModes, []any{expected}) {
			t.Fatalf("approval modes = %v", approvalModes)
		}
		if !strings.Contains(trace, "reasoningEffort") {
			t.Fatal("the effort reaches the host")
		}
		find := func(method string) map[string]any {
			for _, message := range messages {
				if message["method"] == method {
					return message
				}
			}
			return nil
		}
		if scenario.name == "question" {
			method := "userInput/cancel"
			if scenario.choice == "accept" {
				method = "userInput/answer"
			}
			response := find(method)
			if response == nil {
				t.Fatal("Muse received the question response")
			}
			if scenario.choice == "accept" {
				answers, _ := jsonAt(response, "params", "answers").([]any)
				if len(answers) == 0 || jsonAt(answers[0], "selectedLabel") != "First" {
					t.Fatalf("response = %v", response)
				}
			}
		}
		if scenario.name == "permission" {
			decision := find("approval/decide")
			if decision == nil {
				t.Fatal("Muse received permission decision")
			}
			params, _ := json.Marshal(decision["params"])
			if !strings.Contains(string(params), scenario.choice) {
				t.Fatalf("decision = %v", decision)
			}
		}
	}
}

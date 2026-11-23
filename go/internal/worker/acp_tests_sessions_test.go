// Port of mj-worker/src/acp/tests.rs: the tests of clearing a session's
// context, the session instructions each pinned adapter receives, and the
// pending forms a turn's withdrawal and the operator-away policy contend for.

package worker

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Hard-won: 29a80b2d2: Correct bridge startup exceeded the fixture event deadline under load.
func TestClearReplacesTheNativeSessionWithoutForwardingAPrompt(t *testing.T) {
	exerciseContextClear(t, false, core.HarnessKindCodex)
	exerciseContextClear(t, false, core.HarnessKindClaude)
	exerciseContextClear(t, false, core.HarnessKindKimi)
}

// I1-13: a resumed Claude session reports its model as a raw id that its own
// catalogue does not list. Replaying that id after `/clear` must not fail the
// clear or stop the runtime; the new conversation keeps the bridge's model.
// Hard-won: 2416c17: Clear stopped Claude permanently when a bridge no longer listed the saved model.
func TestClearSkipsAReportedModelTheBridgeDoesNotList(t *testing.T) {
	temp := t.TempDir()
	script := filepath.Join(temp, "clear.py")
	if err := os.WriteFile(script, []byte(`
import json, os, sys
root = sys.argv[1]
count_path = os.path.join(root, 'generation')
generation = int(open(count_path).read()) + 1 if os.path.exists(count_path) else 1
with open(count_path, 'w') as f: f.write(str(generation))
current = 'claude-fable-5-1[1m]' if generation == 1 else 'default'
options = [{'id':'model','name':'Model','category':'model','type':'select','currentValue':current,'options':[{'value':'default','name':'Default'},{'value':'opus[1m]','name':'Opus'}]}]
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    ident = request.get('id')
    with open(os.path.join(root, 'requests.jsonl'), 'a') as f:
        f.write(json.dumps(request) + '\n')
    if ident is None: continue
    if method == 'initialize':
        result = {'protocolVersion': 1, 'agentCapabilities': {'loadSession': True}}
    elif method in ('session/new', 'session/load', 'session/resume'):
        result = {'sessionId': 'original' if generation == 1 or method != 'session/new' else 'replacement',
                  'configOptions':options,
                  'modes': {'currentModeId':'auto','availableModes':[{'id':'agent','name':'Agent'},{'id':'plan','name':'Plan'},{'id':'auto','name':'Auto'}]}}
    elif method == 'session/set_config_option':
        value = request['params']['value']
        if value not in ('default', 'opus[1m]'):
            print(json.dumps({'jsonrpc':'2.0','id':ident,'error':{'code':-32603,'message':'unknown model'}}), flush=True)
            continue
        options[0]['currentValue'] = value
        result = {'configOptions':options}
    else:
        result = {}
    print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}), flush=True)
`), 0o644); err != nil {
		t.Fatal(err)
	}
	spec := silentBridgeSpec(core.StallPolicy{})
	spec.Command = "python3"
	spec.Args = []string{script, temp}
	spec.Cwd = temp
	spec.Harness = core.HarnessKindClaude
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	runner := acpTestsStartRun(spec, requests, events)
	defer runner.abort()
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		_, ok := event.(core.RuntimeEventSessionConfigured)
		return ok
	})
	requests <- CommandRequestClearContext{RequestID: "clear-request"}
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		cleared, ok := event.(core.RuntimeEventContextCleared)
		return ok && cleared.RequestID == "clear-request" && cleared.NativeSessionID == "replacement"
	})
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		_, ok := event.(core.RuntimeEventSessionConfigured)
		return ok
	})
	close(requests)
	drained := acpTestsDrain(events, runner)
	if _, err := runner.wait(t, ioTimeout, "the runtime ends with its requests"); err != nil {
		t.Fatal(err)
	}
	<-drained
}

// Hard-won: 29a80b2d2: Correct bridge replacement exceeded the fixture event deadline under load.
func TestFailedClearReloadsThePreviousNativeSession(t *testing.T) {
	exerciseContextClear(t, true, core.HarnessKindCodex)
	exerciseContextClear(t, true, core.HarnessKindClaude)
	exerciseContextClear(t, true, core.HarnessKindKimi)
}

// Opt-in authenticated adapter check; uses a disposable project and never
// opens the controller database. Supply MJ_CLEAR_ADAPTER and
// MJ_CLEAR_HARNESS.
func TestLiveAdapterCompactsAndReplacesContext(t *testing.T) {
	adapter, ok := os.LookupEnv("MJ_CLEAR_ADAPTER")
	if !ok {
		t.Skip("requires an authenticated ACP adapter and makes live provider requests")
	}
	spec := silentBridgeSpec(core.StallPolicy{})
	spec.Command = adapter
	switch harness := os.Getenv("MJ_CLEAR_HARNESS"); harness {
	case "codex":
		spec.Harness = core.HarnessKindCodex
	case "claude":
		spec.Harness = core.HarnessKindClaude
	default:
		t.Fatalf("unsupported live harness: %s", harness)
	}
	spec.Cwd = t.TempDir()
	spec.Verdict = nil
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 256)
	runner := acpTestsStartRun(spec, requests, events)
	defer runner.abort()
	next := func() core.RuntimeEvent {
		t.Helper()
		return acpTestsExpectEvent(t, events, runner, 180*time.Second, "the adapter keeps reporting", "adapter remains connected")
	}
	var original *string
	for configured := false; !configured; {
		switch event := next().(type) {
		case core.RuntimeEventSessionStarted:
			id := event.NativeSessionID
			original = &id
		case core.RuntimeEventSessionConfigured:
			configured = true
		}
	}
	for _, prompt := range []struct{ id, text string }{
		{"hello", "Reply with just hello. Do not use tools."},
		{"compact", "/compact"},
	} {
		requests <- acpTestsPromptRequest(prompt.id, prompt.text)
		for finished := false; !finished; {
			switch event := next().(type) {
			case core.RuntimeEventPromptFinished:
				finished = event.RequestID == prompt.id
			case core.RuntimeEventCommandRejected:
				if event.RequestID == prompt.id {
					t.Fatalf("%s: %s", prompt.id, event.Message)
				}
			}
		}
		t.Logf("live adapter: %s completed", prompt.id)
	}
	requests <- CommandRequestClearContext{RequestID: "clear"}
	for configured := false; !configured; {
		switch event := next().(type) {
		case core.RuntimeEventContextCleared:
			if original != nil && event.NativeSessionID == *original {
				t.Fatal("clear kept the native identity")
			}
			t.Log("live adapter: native identity replaced")
		case core.RuntimeEventSessionConfigured:
			configured = true
		case core.RuntimeEventCommandRejected:
			t.Fatalf("clear: %s", event.Message)
		}
	}
	close(requests)
	drained := acpTestsDrain(events, runner)
	if _, err := runner.wait(t, 180*time.Second, "the runtime ends with its requests"); err != nil {
		t.Fatal(err)
	}
	<-drained
}

// acpTestsAnswerState is oneshot::Receiver::try_recv on a pending form's
// answer: "empty" while it waits, "closed" once its sender was dropped
// without an answer, "answered" when an answer arrived.
func acpTestsAnswerState(receiver *PendingAnswerReceiver) string {
	select {
	case _, ok := <-receiver.C():
		if !ok {
			return "closed"
		}
		return "answered"
	default:
		return "empty"
	}
}

// acpTestsReady is std::future::ready(()): a cancellation already fired.
func acpTestsReady() <-chan struct{} {
	ready := make(chan struct{})
	close(ready)
	return ready
}

// Hard-won: 536a8f8: Escape left Kimi permission forms open after the owning turn was cancelled.
func TestCancellingATurnWithdrawsItsPendingPermissionForms(t *testing.T) {
	// I2-15: after Escape, Kimi left its permission request pending and the
	// form stayed open on an idle session.
	pending := DefaultPendingElicitations()
	permissionID := toolPermissionIDPrefix + "1"
	permission := pending.register(pendingTestQuestion(permissionID), "", AwayRequestTool{Value: "permission"})
	question := pending.register(pendingTestQuestion("elicitation-1"), "", AwayRequestQuestion{})
	withdrawToolPermissions(&pending)
	if state := acpTestsAnswerState(permission); state != "closed" {
		t.Fatalf("the permission's task answers the agent with cancelled: %s", state)
	}
	if state := acpTestsAnswerState(question); state != "empty" {
		t.Fatalf("a question is not a tool permission: %s", state)
	}
	if pending.isEmpty() {
		t.Fatal("the question is still pending")
	}
	if pending.remove("elicitation-1") == nil {
		t.Fatal("the question is still registered")
	}
	if !pending.isEmpty() {
		t.Fatal("nothing is left pending")
	}
}

// Hard-won: 747336f25: AFK answered a permission while a cancelled turn was being withdrawn.
func TestAfkCannotAnswerAPermissionBetweenWithdrawalAndFormRemoval(t *testing.T) {
	owner := &OperatorAway{}
	pending := owner.pendingForBridge()
	id := toolPermissionIDPrefix + "1"
	permission := pending.register(pendingTestQuestion(id), "", AwayRequestTool{Value: "permission"})

	pending.beginWithdrawal()
	owner.set(core.OperatorPresence{Away: true, Revision: 1})
	if state := acpTestsAnswerState(permission); state != "empty" {
		t.Fatalf("the form remains for withdrawal to remove: %s", state)
	}

	pending.withdrawToolPermissions()
	if state := acpTestsAnswerState(permission); state != "closed" {
		t.Fatalf("withdrawal drops the form's answer: %s", state)
	}
}

func TestAcceptedElicitationRepliesSurviveConcurrentWithdrawal(t *testing.T) {
	for _, accepted := range []bool{true, false} {
		pending := DefaultPendingElicitations()
		received := pending.register(pendingTestQuestion("question-1"), "", AwayRequestQuestion{})
		if accepted {
			if err := resolvePendingElicitation(&pending, "question-1", core.ElicitationResponse{
				Action:  core.ElicitationResponseAccept,
				Content: map[string]core.ElicitationValue{},
			}); err != nil {
				t.Fatal(err)
			}
		}
		response := awaitElicitationResponse(context.Background(), &pending, "question-1", received, acpTestsReady())
		if (response != nil) != accepted {
			t.Fatalf("accepted %v: response %#v", accepted, response)
		}
		if !pending.isEmpty() {
			t.Fatal("the form is no longer pending")
		}
		if err := resolvePendingElicitation(&pending, "question-1", core.ElicitationResponse{Action: core.ElicitationResponseCancel}); err == nil {
			t.Fatal("a resolved form cannot be answered again")
		}
	}
}

func TestPendingAfkArbitrationPreservesRegistrationOrderAndBridgeScope(t *testing.T) {
	owner := &OperatorAway{}
	old := owner.pendingForBridge()
	current := owner.pendingForBridge()
	// Lexical id order differs from the order in which the worker accepted them.
	first := old.register(pendingTestQuestion("question-2"), "", AwayRequestQuestion{})
	repeated := current.register(pendingTestQuestion("question-10"), "", AwayRequestQuestion{})
	human := current.register(pendingTestQuestion("human"), "native-child", AwayRequestQuestion{})
	if err := resolvePendingElicitation(&current, "human", core.ElicitationResponse{Action: core.ElicitationResponseDecline}); err != nil {
		t.Fatal(err)
	}
	owner.set(core.OperatorPresence{Away: true, Revision: 1})
	if answer := awaitElicitationResponse(context.Background(), &old, "question-2", first, acpTestsReady()); answer == nil {
		t.Fatal("the first question is answered away")
	} else if _, away := answer.(PendingAnswerOperatorAway); !away {
		t.Fatalf("the first question is answered away: %#v", answer)
	}
	if answer := awaitElicitationResponse(context.Background(), &current, "human", human, acpTestsReady()); answer == nil {
		t.Fatal("the person's answer stands")
	} else if decline, ok := answer.(PendingAnswerHuman); !ok || decline.Value.Action != core.ElicitationResponseDecline {
		t.Fatalf("the person's answer stands: %#v", answer)
	}
	if state := acpTestsAnswerState(repeated); state != "empty" {
		t.Fatalf("one away period answers one question: %s", state)
	}
	// Same ids in different bridges cannot make an old bridge withdraw new work.
	reused := current.register(pendingTestQuestion("question-2"), "", AwayRequestQuestion{})
	old.close()
	if state := acpTestsAnswerState(reused); state != "empty" {
		t.Fatalf("closing the old bridge withdrew the current one's form: %s", state)
	}
	late := old.register(pendingTestQuestion("late"), "another-child", AwayRequestQuestion{})
	if state := acpTestsAnswerState(late); state != "closed" {
		t.Fatalf("a closed bridge registers nothing: %s", state)
	}
	// A missed off revision still starts a new period, and registration order wins.
	owner.set(core.OperatorPresence{Away: true, Revision: 3})
	if answer := awaitElicitationResponse(context.Background(), &current, "question-10", repeated, acpTestsReady()); answer == nil {
		t.Fatal("the next question is answered away")
	} else if _, away := answer.(PendingAnswerOperatorAway); !away {
		t.Fatalf("the next question is answered away: %#v", answer)
	}
	if state := acpTestsAnswerState(reused); state != "empty" {
		t.Fatalf("registration order decides: %s", state)
	}
	current.close()
	if state := acpTestsAnswerState(reused); state != "closed" {
		t.Fatalf("closing the bridge withdraws its form: %s", state)
	}
	next := owner.pendingForBridge()
	restarted := next.register(pendingTestQuestion("question-2"), "", AwayRequestQuestion{})
	if state := acpTestsAnswerState(restarted); state != "empty" {
		t.Fatalf("bridge cleanup retains the period's refusal memory: %s", state)
	}
}

// acpTestsDropPending drops a pending entry the caller took, as Rust drops
// the PendingElicitation: an ACP responder's answer sender goes with it.
func acpTestsDropPending(entry *PendingElicitation) {
	if entry == nil {
		return
	}
	switch responder := entry.responder.(type) {
	case PendingResponderAcp:
		responder.answer.drop()
	case *PendingResponderAcp:
		responder.answer.drop()
	}
}

func TestPermissionCancelAndAfkClaimThePendingEntryOnce(t *testing.T) {
	for _, cancelFirst := range []bool{true, false} {
		owner := &OperatorAway{}
		pending := owner.pendingForBridge()
		id := toolPermissionIDPrefix + "1"
		received := pending.register(pendingTestQuestion(id), "", AwayRequestTool{Value: "command"})
		cancel := core.ElicitationResponse{Action: core.ElicitationResponseCancel}
		var cancelled *PendingElicitation
		if cancelFirst {
			cancelled = pending.takeTurnCancel(id, &cancel)
			if cancelled == nil {
				t.Fatal("the turn's cancel claims the form")
			}
		}
		owner.set(core.OperatorPresence{Away: true, Revision: 1})
		if pending.takeTurnCancel(id, &cancel) != nil {
			t.Fatal("the form is claimed once")
		}
		if err := resolvePendingElicitation(&pending, id, core.ElicitationResponse{Action: core.ElicitationResponseDecline}); err == nil {
			t.Fatal("a claimed form cannot be answered")
		}
		// The cancellation owner retains the sender until session/cancel is sent.
		acpTestsDropPending(cancelled)
		answer := awaitElicitationResponse(context.Background(), &pending, id, received, acpTestsReady())
		_, away := answer.(PendingAnswerOperatorAway)
		if away != !cancelFirst {
			t.Fatalf("cancel first %v: answer %#v", cancelFirst, answer)
		}
	}
}

// acpTestsReplies is a recorded adapter's replies, by method.
type acpTestsReplies map[string]any

func (r acpTestsReplies) clone(t *testing.T) acpTestsReplies {
	t.Helper()
	cloned := acpTestsReplies{}
	for method, reply := range r {
		cloned[method] = acpTestsPlain(t, reply)
	}
	return cloned
}

// acpTestsObject is serde_json's IndexMut on an object: the member at path,
// created as an empty object where it is missing or null.
func acpTestsObject(t *testing.T, value map[string]any, path ...string) map[string]any {
	t.Helper()
	for _, key := range path {
		next, present := value[key]
		if !present || next == nil {
			created := map[string]any{}
			value[key] = created
			value = created
			continue
		}
		object, ok := next.(map[string]any)
		if !ok {
			t.Fatalf("%s is not an object: %v", key, next)
		}
		value = object
	}
	return value
}

// Replay pinned-adapter lifecycle replies (Claude source-derived) against real driver requests.
// Hard-won: 621276a78: the unsupported-adapter branch had no phase that reached it.
// Hard-won: 5fc951f2e: shared Claude profiles were told to use unregistered tools.
func TestSessionInstructionsSystemPromptContractReplaysPinnedAdapters(t *testing.T) {
	for _, pinned := range []struct {
		harness   core.HarnessKind
		recording string
	}{
		{core.HarnessKindKimi, "kimi-opens.jsonl"},
		{core.HarnessKindOpenCode, "opencode-opens.jsonl"},
		{core.HarnessKindGrok, "grok-opens.jsonl"},
		{core.HarnessKindClaude, "claude-opens.jsonl"},
		{core.HarnessKindCodex, "codex-opens.jsonl"},
		{core.HarnessKindMuse, "muse-opens.jsonl"},
		{core.HarnessKindDsh, "dsh-opens.jsonl"},
		{core.HarnessKindAntigravity, "antigravity-opens.jsonl"},
	} {
		harness := pinned.harness
		recording, err := os.ReadFile(filepath.Join("testdata", "acp", "project-memory", pinned.recording))
		if err != nil {
			t.Fatal(err)
		}
		recorded := acpTestsReplies{}
		for _, line := range strings.Split(strings.TrimRight(string(recording), "\n"), "\n") {
			value, err := acpTestsDecodeObject([]byte(line))
			if err != nil {
				t.Fatal(err)
			}
			method, _ := acpTestsString(value["method"])
			recorded[method] = value["result"]
		}
		for _, phase := range []string{
			"new",
			"unregistered-profile",
			"load",
			"resume",
			"clear",
			"unreadable-load",
			"fifo-load",
			"unsupported-adapter",
			"unsupported-brief-writer",
			"unsupported-reviewer",
		} {
			if phase == "unregistered-profile" && harness != core.HarnessKindClaude {
				continue
			}
			if phase == "fifo-load" && !acpTestsHasFifos {
				continue
			}
			resumed := phase == "load" || phase == "resume" || phase == "unreadable-load" || phase == "fifo-load" || phase == "unsupported-adapter"
			temp := t.TempDir()
			nativeFile := harness == core.HarnessKindKimi || harness == core.HarnessKindOpenCode || harness == core.HarnessKindGrok
			if nativeFile && (phase == "unsupported-adapter" || phase == "unsupported-brief-writer" ||
				phase == "unsupported-reviewer" || phase == "unreadable-load" || phase == "fifo-load") {
				continue
			}
			stagedHome := harness.HomeFromEnvironment(filepath.Join(temp, "profile"))
			if err := os.MkdirAll(stagedHome, 0o755); err != nil {
				t.Fatal(err)
			}
			nativePath := filepath.Join(stagedHome, harness.AgentInstructionsFile())
			if err := os.WriteFile(nativePath, []byte("user profile instructions\r\n"), 0o644); err != nil {
				t.Fatal(err)
			}
			spec := reloadFallbackSpec(harness)
			spec.Environment[harness.HomeEnv()] = filepath.Join(temp, "profile")
			spec.ResumeSession = nil
			if resumed {
				spec.ResumeSession = acpTestsStringPointer("native")
			}
			spec.InstructionRole = nil
			switch phase {
			case "unsupported-brief-writer":
				role := core.InstructionRoleBriefWriter
				spec.InstructionRole = &role
			case "unsupported-reviewer":
				role := core.InstructionRoleReviewer
				spec.InstructionRole = &role
			}
			socket := workerSocket(core.SubagentMcpRoleMessageOnly)
			socket.ProfileRegistration = harness == core.HarnessKindClaude && phase != "unregistered-profile"
			socket.ReviewTools = true
			spec.SubagentMCPSocket = &socket
			spec.ProjectMemory = &core.ProjectMemoryLaunchConfig{
				ProjectKey:      "test",
				Root:            filepath.Join(temp, "memory"),
				BaselineRoot:    filepath.Join(temp, "baseline"),
				RepositoryRoots: map[string]string{},
				MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
			}
			index := filepath.Join(spec.ProjectMemory.Root, core.MemoryIndex)
			if phase == "unreadable-load" {
				if err := os.MkdirAll(filepath.Dir(index), 0o755); err != nil {
					t.Fatal(err)
				}
				if err := os.WriteFile(index, []byte{0xff}, 0o644); err != nil {
					t.Fatal(err)
				}
			}
			if phase == "fifo-load" {
				if err := os.MkdirAll(filepath.Dir(index), 0o755); err != nil {
					t.Fatal(err)
				}
				if err := acpTestsMkfifo(index); err != nil {
					t.Fatal(err)
				}
			}
			expectedEvents := make(chan core.RuntimeEvent, 256)
			expected, err := compose(context.Background(), &spec, true, expectedEvents)
			if err != nil {
				t.Fatal(err)
			}
			if phase == "unregistered-profile" {
				if strings.Contains(expected, "send_message") || strings.Contains(expected, "request_review") {
					t.Fatalf("an unregistered profile is not told about its tools: %s", expected)
				}
			} else if harness == core.HarnessKindClaude {
				if !strings.Contains(expected, "send_message") || !strings.Contains(expected, "request_review") {
					t.Fatalf("a registered profile is told about its tools: %s", expected)
				}
			}
			if phase == "clear" {
				spec.ClearContextRequest = &ContextReset{RequestID: "clear"}
			}
			replies := recorded.clone(t)
			initialize, _ := replies["initialize"].(map[string]any)
			if initialize == nil {
				t.Fatalf("%v recorded no initialize reply", harness)
			}
			switch {
			case phase == "load" || phase == "unreadable-load" || phase == "fifo-load":
				capabilities, ok := acpTestsGet(initialize, "agentCapabilities", "sessionCapabilities").(map[string]any)
				if !ok {
					t.Fatalf("%v records its session capabilities", harness)
				}
				delete(capabilities, "resume")
			case phase == "resume" && (harness == core.HarnessKindCodex || harness == core.HarnessKindClaude):
				// Isolate the resume builder from Codex's native-child replay policy.
				acpTestsObject(t, initialize, "_meta", "jetbrains", "air")["capabilities"] = []any{}
			}
			if phase == "unsupported-adapter" || phase == "unsupported-brief-writer" || phase == "unsupported-reviewer" {
				capabilities := acpTestsObject(t, initialize, "agentCapabilities")
				sessionCapabilities, ok := capabilities["sessionCapabilities"].(map[string]any)
				if !ok {
					t.Fatalf("%v records its session capabilities", harness)
				}
				delete(sessionCapabilities, "systemPrompt")
				if meta, ok := capabilities["_meta"].(map[string]any); ok {
					delete(meta, "antigravity")
				}
			}
			adapterVersion, ok := acpTestsString(acpTestsGet(initialize, "agentInfo", "version"))
			if !ok {
				adapterVersion = "(unknown version)"
			}
			client, peer := acpTestsDuplex(64 * 1024)
			bridgeDone := acpTestsGo(func() {
				bridge := acpTestsBridge(peer)
				sawOpen := false
				for {
					request, ok := bridge.next(t)
					if !ok {
						break
					}
					method, _ := acpTestsString(request["method"])
					id, present := request["id"]
					if !present {
						continue
					}
					replyMethod := method
					if method == "session/load" {
						replyMethod = "session/resume"
					}
					result, recordedReply := replies[replyMethod]
					if !recordedReply {
						result = map[string]any{}
					}
					result = acpTestsPlain(t, result)
					if method == "session/set_config_option" {
						options := acpTestsArray(acpTestsPlain(t, acpTestsGet(replies["session/new"], "configOptions")))
						for _, option := range options {
							if acpTestsGet(option, "id") == acpTestsGet(request, "params", "configId") {
								option.(map[string]any)["currentValue"] = acpTestsGet(request, "params", "value")
							}
						}
						result = map[string]any{"configOptions": options}
					}
					if method == "session/new" || method == "session/resume" || method == "session/load" {
						systemPrompt := acpTestsGet(request, "params", "systemPrompt")
						antigravityPrompt := acpTestsGet(request, "params", "_meta", "antigravity", "systemPrompt")
						switch {
						case phase == "unsupported-adapter":
							if systemPrompt != nil || antigravityPrompt != nil {
								t.Errorf("%v %s: an adapter without system prompts got one: %v", harness, method, request)
							}
							if contents, _ := os.ReadFile(nativePath); string(contents) != "user profile instructions\r\n" {
								t.Errorf("%v: the profile's instructions changed: %q", harness, contents)
							}
						case nativeFile:
							if systemPrompt != nil {
								t.Errorf("%v %s: a native-file adapter got a system prompt: %v", harness, method, request)
							}
							contents, _ := os.ReadFile(nativePath)
							if !strings.HasPrefix(string(contents), "user profile instructions\r\n") || string(contents) != "user profile instructions\r\n" {
								t.Errorf("%v: the profile's instructions changed: %q", harness, contents)
							}
							if expected != "" {
								t.Errorf("%v: a native-file adapter's instructions are empty: %q", harness, expected)
							}
						case harness == core.HarnessKindAntigravity:
							if got := acpTestsGet(antigravityPrompt, "append"); got != expected {
								t.Errorf("%v %s: appended %v, want %q", harness, method, got, expected)
							}
							if systemPrompt != nil {
								t.Errorf("%v %s: a standard system prompt too: %v", harness, method, request)
							}
						case expected == "":
							if systemPrompt != nil {
								t.Errorf("%v %s: empty instructions sent a system prompt: %v", harness, method, request)
							}
						default:
							want := map[string]any{"mode": "append", "content": []any{map[string]any{"type": "text", "text": expected}}}
							if !acpTestsJSONEqual(t, systemPrompt, want) {
								t.Errorf("%v %s: system prompt %v, want %v", harness, method, systemPrompt, want)
							}
						}
						sawOpen = true
					}
					if !bridge.send(acpTestsResult(id, result)) {
						t.Errorf("%v %s: write the reply", harness, phase)
						return
					}
				}
				if wantOpen := phase != "unsupported-brief-writer" && phase != "unsupported-reviewer"; sawOpen != wantOpen {
					t.Errorf("%v %s: saw a session open %v, want %v", harness, phase, sawOpen, wantOpen)
				}
			})
			requests := make(chan CommandRequest, 4)
			events := make(chan core.RuntimeEvent, 256)
			driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
			if phase == "unsupported-brief-writer" || phase == "unsupported-reviewer" {
				for {
					event, closed, timedOut := acpTestsNext(events, driver, ioTimeout)
					if timedOut {
						t.Fatalf("%v %s: the runtime keeps reporting", harness, phase)
					}
					if closed {
						break
					}
					switch event.(type) {
					case core.RuntimeEventSessionStarted, core.RuntimeEventSessionConfigured:
						t.Fatalf("%v %s: a session that cannot take its instructions opened: %#v", harness, phase, event)
					}
				}
				_, err := driver.wait(t, ioTimeout, "the runtime ends")
				if err == nil {
					t.Fatalf("%v %s: the role must refuse an adapter without system prompts", harness, phase)
				}
				want := "Cannot start the relation brief writer"
				if phase == "unsupported-reviewer" {
					want = "Cannot start the automated reviewer"
				}
				if message := err.Error(); !strings.Contains(message, want) || !strings.Contains(message, harness.DisplayName()) ||
					!strings.Contains(message, adapterVersion) {
					t.Fatal(message)
				}
				close(requests)
				client.Close()
				acpTestsReceive(t, bridgeDone, ioTimeout, "the bridge ends with its input")
				continue
			}
			indexWarning, deliveryWarning := false, false
			for configured := false; !configured; {
				event, closed, timedOut := acpTestsNext(events, driver, ioTimeout)
				if timedOut {
					t.Fatalf("%v %s: the runtime keeps reporting", harness, phase)
				}
				if closed {
					t.Fatalf("%v %s: %s", harness, phase, driver.describe())
				}
				switch event := event.(type) {
				case core.RuntimeEventWarning:
					if strings.Contains(event.Message, "standing instructions and project memory could not be delivered") {
						if !strings.Contains(event.Message, harness.DisplayName()) || !strings.Contains(event.Message, adapterVersion) ||
							!strings.Contains(event.Message, "Updating the adapter") {
							t.Fatal(event.Message)
						}
						deliveryWarning = true
					}
					if strings.Contains(event.Message, "Project memory index") && strings.Contains(event.Message, "unavailable") {
						reason := "UTF-8"
						if phase == "fifo-load" {
							reason = "regular file"
						}
						if !strings.Contains(event.Message, index) || !strings.Contains(event.Message, reason) {
							t.Fatal(event.Message)
						}
						indexWarning = true
					}
				case core.RuntimeEventContextCleared:
					if event.Memory != nil {
						t.Fatalf("a clear without memory reports none: %q", *event.Memory)
					}
				case core.RuntimeEventSessionConfigured:
					configured = true
				}
			}
			if want := (phase == "unreadable-load" || phase == "fifo-load") && harness != core.HarnessKindClaude; indexWarning != want {
				t.Fatalf("%v %s: index warning %v, want %v", harness, phase, indexWarning, want)
			}
			if want := phase == "unsupported-adapter"; deliveryWarning != want {
				t.Fatalf("%v %s: delivery warning %v, want %v", harness, phase, deliveryWarning, want)
			}
			close(requests)
			drained := acpTestsDrain(events, driver)
			if _, err := driver.wait(t, ioTimeout, "the runtime ends with its requests"); err != nil {
				t.Fatalf("%v %s: %v", harness, phase, err)
			}
			<-drained
			client.Close()
			acpTestsReceive(t, bridgeDone, ioTimeout, "the bridge ends with its input")
		}
	}
}

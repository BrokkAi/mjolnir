// Port of mj-worker/src/acp/tests.rs: its #[cfg(unix)] items, including
// mod terminals.

//go:build unix

package worker

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	goruntime "runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"
	"unicode"
	"weak"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
)

// acpTestsHasFifos says whether this platform has the FIFOs the fifo-load
// phase stalls a read on (Rust: cfg(unix)).
const acpTestsHasFifos = true

func acpTestsMkfifo(path string) error {
	return syscall.Mkfifo(path, 0o600)
}

// acpTestsPythonPath is a path as a Python string literal (Rust formats the
// PathBuf with {:?}, whose quoting Python also reads).
func acpTestsPythonPath(t *testing.T, path string) string {
	t.Helper()
	encoded, err := json.Marshal(path)
	if err != nil {
		t.Fatal(err)
	}
	return string(encoded)
}

func acpTestsWriteScript(t *testing.T, path, source string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(source), 0o644); err != nil {
		t.Fatal(err)
	}
}

// Hard-won: 3f3f3927b: updates sent right behind the `session/load` response were judged by a flag the session task flipped later, so a live turn start and reply were dropped as replay.
func TestUpdatesRightBehindTheLoadResponseAreLiveAndThoseBeforeItAreReplay(t *testing.T) {
	for _, nativeChildren := range []bool{true, false} {
		temp := t.TempDir()
		script := filepath.Join(temp, "load_boundary.py")
		native := "False"
		if nativeChildren {
			native = "True"
		}
		acpTestsWriteScript(t, script, strings.ReplaceAll(`
import json, sys

NATIVE_CHILDREN = __NATIVE__

def emit(value):
    print(json.dumps(value), flush=True)

def update(value):
    emit({'jsonrpc':'2.0', 'method':'session/update', 'params':{'sessionId':'native', 'update':value}})

for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    ident = request.get('id')
    if method == 'initialize':
        caps = ['nativeSubagentSessions'] if NATIVE_CHILDREN else []
        result = {'protocolVersion':1, 'agentCapabilities':{'loadSession':True},
            '_meta':{'jetbrains':{'air':{'version':1,'capabilities':caps}}}}
    elif method == 'session/load':
        update({'sessionUpdate':'async_task_spawned','taskType':'turn','asyncTaskId':'old-turn'})
        update({'sessionUpdate':'agent_message_chunk', 'content':{'type':'text','text':'old history'}})
        emit({'jsonrpc':'2.0','id':ident,'result':{'sessionId':'native',
            'modes':{'currentModeId':'default','availableModes':[{'id':m,'name':m} for m in ('default','acceptEdits','plan','auto')]}}})
        update({'sessionUpdate':'async_task_spawned','taskType':'turn','asyncTaskId':'live-turn'})
        update({'sessionUpdate':'agent_message_chunk', 'content':{'type':'text','text':'live reply'}})
        continue
    else:
        result = {}
    if ident is not None:
        emit({'jsonrpc':'2.0','id':ident,'result':result})
`, "__NATIVE__", native))
		requests := make(chan CommandRequest, 4)
		// A channel that holds one event and is not read at first keeps the
		// session task waiting on its own sends while the notification
		// handler runs, which is the order that exposed the race.
		events := make(chan core.RuntimeEvent, 1)
		spec := acpTestsSpec(core.HarnessKindClaude, "python3", temp)
		spec.Args = []string{script}
		spec.ResumeSession = acpTestsStringPointer("native")
		spec.NativeSessionHistory = NativeSessionHistoryMayHaveHistory
		runner := acpTestsStartRun(spec, requests, events)
		time.Sleep(500 * time.Millisecond)
		var trace []string
		for {
			event, closed, timedOut := acpTestsNext(events, runner, 2*time.Second)
			if closed || timedOut {
				break
			}
			switch event := event.(type) {
			case core.RuntimeEventAirTaskChanged:
				if spawned, ok := event.Update.(core.AirTaskUpdateTurnSpawned); ok {
					trace = append(trace, fmt.Sprintf("turn %s replayed=%v", spawned.TaskID, spawned.Replayed))
				}
			case core.RuntimeEventSessionUpdate:
				update, _ := acpTestsUpdateValue(t, event)
				if text, ok := acpTestsString(acpTestsGet(update, "content", "text")); ok {
					trace = append(trace, "text "+text)
				}
			case core.RuntimeEventNativeAgent:
				if _, ok := event.Event.(core.NativeAgentEventReplayCommit); ok {
					trace = append(trace, "replay-commit")
				}
			}
		}
		close(requests)
		runner.abort()
		acpTestsDrain(events, runner)
		runner.waitBriefly(ioTimeout)
		expected := []string{"turn old-turn replayed=true"}
		if nativeChildren {
			expected = append(expected, "replay-commit")
		}
		expected = append(expected, "turn live-turn replayed=false", "text live reply")
		if !slices.Equal(trace, expected) {
			t.Fatalf("native_children=%v: trace %q, want %q", nativeChildren, trace, expected)
		}
	}
}

// Hard-won: 61a1cfa: Codex replay dropped the current available-command catalogue and goal metadata, including on large history.
// Hard-won: #1443: loading a session presented historical quota failures as new errors.
func TestLoadingNativeHistoryPreservesCurrentMetadataWithoutReplayingFailures(t *testing.T) {
	for _, harness := range []core.HarnessKind{core.HarnessKindCodex, core.HarnessKindClaude} {
		temp := t.TempDir()
		script := filepath.Join(temp, "load_metadata.py")
		acpTestsWriteScript(t, script, `
import json, sys

def emit(value):
    print(json.dumps(value), flush=True)

def update(value):
    emit({'jsonrpc':'2.0', 'method':'session/update', 'params':{'sessionId':'native', 'update':value}})

goal = {'objective':'finish', 'status':'complete', 'createdAt':1, 'tokensUsed':42}
execution = {'version':1, 'revision':1, 'status':'idle'}
failure = {'id':'quota-error','revision':1,'category':'limit','severity':'error',
    'title':"You've hit your session limit · resets 5:20pm (UTC)",'actions':[]}
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    ident = request.get('id')
    if method == 'initialize':
        result = {'protocolVersion':1, 'agentCapabilities':{'loadSession':True, 'sessionCapabilities':{'resume':{}}},
            '_meta':{'jetbrains':{'air':{'version':1,'capabilities':['nativeSubagentSessions'],
                'goal':{'version':1,'controlMethod':'_session/goal','actions':['pause','resume','clear']}}}}}
    elif method in ('session/new', 'session/resume', 'session/load'):
        assert method == 'session/load', request
        update({'sessionUpdate':'agent_message_chunk', 'content':{'type':'text','text':'old history ' * 12000}})
        update({'sessionUpdate':'available_commands_update', 'availableCommands':[{'name':'goal','description':'Set a goal'}]})
        update({'sessionUpdate':'tool_call','toolCallId':'old-tool','title':'old call','status':'in_progress'})
        update({'sessionUpdate':'session_info_update','_meta':{'jetbrains':{'air':{'sessionFailure':failure}}}})
        update({'sessionUpdate':'session_info_update','title':'Current title','_meta':{'jetbrains':{'air':{'goal':goal,'sessionFailure':failure}},'execution':execution}})
        update({'sessionUpdate':'usage_update','used':100,'size':1000})
        update({'sessionUpdate':'config_option_update','configOptions':[
            {'id':'model','name':'Model','type':'select','currentValue':'astra','options':[{'value':'astra','name':'Astra'}]}]})
        update({'sessionUpdate':'current_mode_update','currentModeId':'agent'})
        update({'sessionUpdate':'plan','entries':[{'content':'old plan','priority':'medium','status':'completed'}]})
        result = {'sessionId':'native', 'modes':{'currentModeId':'agent','availableModes':[{'id':'agent','name':'Guardian'},{'id':'auto','name':'Auto'}]},
            '_meta':{'jetbrains':{'air':{'goal':goal}},'execution':execution}}
    elif method == 'session/prompt':
        update({'sessionUpdate':'session_info_update','_meta':{'jetbrains':{'air':{'sessionFailure':failure}}}})
        update({'sessionUpdate':'tool_call_update','toolCallId':'old-tool','status':'completed'})
        update({'sessionUpdate':'tool_call','toolCallId':'new-tool','title':'new call','status':'in_progress'})
        update({'sessionUpdate':'tool_call_update','toolCallId':'new-tool','status':'completed'})
        update({'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'live output ' * 12000}})
        result = {'stopReason':'end_turn'}
    else:
        result = {}
    if ident is not None:
        emit({'jsonrpc':'2.0','id':ident,'result':result})
`)
		requests := make(chan CommandRequest, 4)
		events := make(chan core.RuntimeEvent, 64)
		spec := acpTestsSpec(harness, "python3", temp)
		spec.Args = []string{script}
		spec.ResumeSession = acpTestsStringPointer("native")
		spec.NativeSessionHistory = NativeSessionHistoryMayHaveHistory
		runner := acpTestsStartRun(spec, requests, events)
		defer runner.abort()
		var updates, beforeLoadResponse []any
		started, replayCommitted := false, false
		for done := false; !done; {
			event := acpTestsExpectEvent(t, events, runner, 10*time.Second,
				"adapter must not stall on a full output pipe", "runtime remains connected")
			switch event := event.(type) {
			case core.RuntimeEventSessionUpdate:
				update, _ := acpTestsUpdateValue(t, event)
				if !replayCommitted {
					beforeLoadResponse = append(beforeLoadResponse, update)
				}
				updates = append(updates, update)
			case core.RuntimeEventNativeAgent:
				if _, ok := event.Event.(core.NativeAgentEventReplayCommit); ok {
					replayCommitted = true
				}
			case core.RuntimeEventSessionStarted:
				if !event.Resumed {
					t.Fatalf("%v: the session must be resumed", harness)
				}
				started = true
			case core.RuntimeEventSessionConfigured:
				if started {
					requests <- acpTestsPromptRequest("new-turn", "continue")
					started = false
				}
			case core.RuntimeEventPromptFinished:
				done = event.RequestID == "new-turn"
			case core.RuntimeEventStopped:
				runner.waitBriefly(ioTimeout)
				t.Fatalf("%v: runtime stopped while loading the session: %s", harness, runner.describe())
			}
		}
		close(requests)
		if _, err := runner.wait(t, 10*time.Second, "the runtime ends with its requests"); err != nil {
			t.Fatal(err)
		}
		if !replayCommitted {
			t.Fatalf("%v: the replay was committed", harness)
		}
		for _, update := range beforeLoadResponse {
			if _, present := acpTestsPointer(update, "/_meta/jetbrains/air/sessionFailure"); present {
				t.Fatalf("%v: historical failures must not become current transcript notices", harness)
			}
		}
		var liveFailures []any
		for _, update := range updates {
			if failure, present := acpTestsPointer(update, "/_meta/jetbrains/air/sessionFailure"); present {
				liveFailures = append(liveFailures, failure)
			}
		}
		if len(liveFailures) != 1 {
			t.Fatalf("%v: live failures still pass: %v", harness, liveFailures)
		}
		if acpTestsGet(liveFailures[0], "id") != "quota-error" {
			t.Fatalf("live failure %v", liveFailures[0])
		}
		some := func(updates []any, matches func(any) bool) bool { return slices.ContainsFunc(updates, matches) }
		if !some(beforeLoadResponse, func(u any) bool { return acpTestsGet(u, "availableCommands", 0, "name") == "goal" }) {
			t.Fatalf("%v: the command catalogue survives replay", harness)
		}
		if !some(beforeLoadResponse, func(u any) bool {
			return acpTestsGet(u, "_meta", "jetbrains", "air", "goal", "tokensUsed") == float64(42)
		}) {
			t.Fatalf("%v: the goal survives replay", harness)
		}
		if !some(beforeLoadResponse, func(u any) bool { return acpTestsGet(u, "configOptions", 0, "currentValue") == "astra" }) {
			t.Fatalf("%v: the config options survive replay", harness)
		}
		if !some(beforeLoadResponse, func(u any) bool { return acpTestsGet(u, "currentModeId") == "agent" }) {
			t.Fatalf("%v: the current mode survives replay", harness)
		}
		if !some(beforeLoadResponse, func(u any) bool { return acpTestsGet(u, "title") == "Current title" }) {
			t.Fatalf("%v: the title survives replay", harness)
		}
		if some(updates, func(u any) bool { return acpTestsGet(u, "toolCallId") == "old-tool" }) {
			t.Fatalf("%v: a replayed tool call reached the relay", harness)
		}
		if some(updates, func(u any) bool {
			kind := acpTestsGet(u, "sessionUpdate")
			return kind == "plan" || kind == "usage_update"
		}) {
			t.Fatalf("%v: replayed plans and usage reached the relay", harness)
		}
		if !some(updates, func(u any) bool {
			return acpTestsGet(u, "toolCallId") == "new-tool" && acpTestsGet(u, "status") == "completed"
		}) {
			t.Fatalf("%v: the live tool call completes", harness)
		}
		var text strings.Builder
		for _, update := range updates {
			if chunk, ok := acpTestsString(acpTestsGet(update, "content", "text")); ok {
				text.WriteString(chunk)
			}
		}
		if text.String() != strings.Repeat("live output ", 12000) {
			t.Fatalf("%v: the relayed text is %d bytes", harness, text.Len())
		}
	}
}

// acpTestsFionread is the FIONREAD ioctl: Linux's TIOCINQ, or the BSD value.
var acpTestsFionread = func() uint {
	if goruntime.GOOS == "linux" {
		return 0x541B
	}
	return 0x4004667F
}()

// Hard-won: #1515: a cancelled spawn_blocking read still held Tokio runtime shutdown.
func TestAStalledLoginReadDoesNotHoldRuntimeShutdown(t *testing.T) {
	home := t.TempDir()
	path := filepath.Join(home, ".credentials.json")
	gate := stalledReadFile(path)
	// Keep the read blocked until the shutdown result is known. Release it on
	// failure too, so the fixture never strands its reader.
	defer gate.Close()
	if _, err := gate.Write([]byte("{")); err != nil {
		t.Fatal(err)
	}
	ctx, stop := context.WithCancel(context.Background())
	defer stop()
	exited := make(chan error, 1)
	go func() {
		_, err := readClaudeLoginFingerprint(ctx, path)
		exited <- err
	}()
	testSupportWaitFor("login reader to consume the FIFO header", func() struct{} {
		for {
			bytes, err := unix.IoctlGetInt(int(gate.Fd()), acpTestsFionread)
			if err != nil {
				panic(err)
			}
			if bytes == 0 {
				return struct{}{}
			}
			goruntime.Gosched()
		}
	})
	if err, returned := acpTestsTryRecv(exited); returned {
		t.Fatalf("the gated read returned: %v", err)
	}
	stop()
	select {
	case <-exited:
	case <-time.After(ioTimeout):
		t.Fatal("a stalled credential read held runtime shutdown")
	}
}

// Hard-won: #1515: supplemental credential evidence became a prerequisite for every prompt.
func TestAPromptDoesNotWaitForAStalledLoginObservation(t *testing.T) {
	home := t.TempDir()
	path := filepath.Join(home, ".credentials.json")
	gate := stalledReadFile(path)
	defer gate.Close()
	observationContext, abortObservation := context.WithCancel(context.Background())
	defer abortObservation()
	observation := make(chan error, 1)
	go func() {
		_, err := readClaudeLoginFingerprint(observationContext, path)
		observation <- err
	}()
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	identity := &ClaudeLoginIdentity{}
	prompts := make(chan int, 1)
	go func() { prompts <- scriptedBridge(t, bridgeStream, filepath.Join(home, "harness-login"), identity) }()
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	spec := silentBridgeSpec(core.StallPolicy{})
	spec.Harness = core.HarnessKindClaude
	spec.LoginIdentity = identity
	spec.Environment["CLAUDE_CONFIG_DIR"] = home
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
	defer driver.abort()
	requests <- acpTestsPromptRequest("native-work", "work")
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		finished, ok := event.(core.RuntimeEventPromptFinished)
		return ok && finished.RequestID == "native-work" && finished.StopReason == "error" &&
			finished.Diagnostic != nil && finished.Diagnostic.LoginFingerprint == nil
	})
	close(requests)
	if _, err := driver.wait(t, ioTimeout, "timed out waiting for prompt dispatch with unavailable credential evidence"); err != nil {
		t.Fatal(err)
	}
	client.Close()
	if count := acpTestsReceive(t, prompts, ioTimeout, "the bridge ends with its input"); count != 1 {
		t.Fatalf("the harness must receive the prompt without waiting for metadata: %d prompts", count)
	}
	if err, returned := acpTestsTryRecv(observation); returned {
		t.Fatalf("the stalled observation returned before it was aborted: %v", err)
	}
	abortObservation()
	if err := acpTestsReceive(t, observation, ioTimeout, "the aborted observation returns"); err == nil {
		t.Fatal("an aborted observation reports its cancellation")
	}
}

// Hard-won: 14b4b00de: a replacement bridge exiting before its first prompt reloaded the unwritten thread and aborted
func TestUnusedCodexBridgeRestartsWithANewThread(t *testing.T) {
	codexBridgeRestartPreservesOnlyUsedThreads(t, false)
}

func TestCodexBridgeRestartsWithTheUsedThreadEvenIfThePromptReplyWasLost(t *testing.T) {
	codexBridgeRestartPreservesOnlyUsedThreads(t, true)
}

// acpTestsOpened is one SessionStarted as the restart tests compare it.
type acpTestsOpened struct {
	nativeSessionID string
	resumed         bool
	replaced        string
}

func codexBridgeRestartPreservesOnlyUsedThreads(t *testing.T, sendPrompt bool) {
	t.Helper()
	temp := t.TempDir()
	launches := filepath.Join(temp, "launches")
	exitRequested := filepath.Join(temp, "exit-requested")
	script := filepath.Join(temp, "codex_persistence.py")
	used := "False"
	if sendPrompt {
		used = "True"
	}
	acpTestsWriteScript(t, script, strings.NewReplacer(
		"__LAUNCHES__", acpTestsPythonPath(t, launches),
		"__EXIT_REQUESTED__", acpTestsPythonPath(t, exitRequested),
		"__USED__", used,
	).Replace(`
import json, os, sys, time
launches = __LAUNCHES__
exit_requested = __EXIT_REQUESTED__
used = __USED__
launch = int(open(launches).read()) + 1 if os.path.exists(launches) else 1
with open(launches, "w") as counter:
    counter.write(str(launch))

def write(payload):
    print(json.dumps(payload), flush=True)

for line in sys.stdin:
    request = json.loads(line)
    ident = request.get("id")
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}, "_meta": {"jetbrains": {"air": {"version": 1, "capabilities": ["nativeSubagentSessions"]}}}}
    elif method in ("session/new", "session/load", "session/resume"):
        if not used and method != "session/new":
            thread = request["params"]["sessionId"]
            write({"jsonrpc": "2.0", "id": ident, "error": {"code": -32603, "message": "Internal error", "data": {"details": f"thread not loaded: {thread}"}}})
            continue
        assert method == ("session/load" if launch > 1 and used else "session/new"), request
        result = {"sessionId": "original" if used else f"thread-{launch}",
                   "modes": {"currentModeId": "agent", "availableModes": [{"id": "agent", "name": "Guardian"}]}}
        write({"jsonrpc": "2.0", "id": ident, "result": result})
        continue
    elif method == "session/prompt":
        if used:
            assert launch == 1
            # Crash after consuming the prompt, before replying.
            break
        assert launch == 2 and request["params"]["sessionId"] == "thread-2", request
        write({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": request["params"]["sessionId"],
            "update": {"sessionUpdate": "agent_message_chunk",
                       "content": {"type": "text", "text": "recovered reply"}}}})
        result = {"stopReason": "end_turn"}
    else:
        result = {}
    if ident is not None:
        write({"jsonrpc": "2.0", "id": ident, "result": result})
    if method == "session/set_mode" and not used and launch == 1:
        # Wait until the runtime has published this open before exiting.
        while not os.path.exists(exit_requested):
            time.sleep(0.01)
        break
`))
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	spec := acpTestsSpec(core.HarnessKindCodex, "python3", temp)
	spec.Args = []string{script}
	if sendPrompt {
		spec.NativeSessionHistory = NativeSessionHistoryLoadBeforeReplace
	} else {
		spec.ResumeSession = acpTestsStringPointer("unused")
		spec.NativeSessionHistory = NativeSessionHistoryCreatedHereUnused
	}
	runner := acpTestsStartRun(spec, requests, events)
	defer runner.abort()
	var opened []acpTestsOpened
	promptSent := false
	var replay []string
	var restartMessages []string
	for done := false; !done; {
		event := acpTestsExpectEvent(t, events, runner, 10*time.Second,
			"the replacement bridge must become ready", "the runtime must survive the first bridge's death")
		switch event := event.(type) {
		case core.RuntimeEventNativeAgent:
			replay = append(replay, fmt.Sprintf("%T", event.Event))
		case core.RuntimeEventHarnessRestarting:
			restartMessages = append(restartMessages, event.Message)
		case core.RuntimeEventSessionStarted:
			replaced := ""
			if event.ReplacedUnusedNativeSessionID != nil {
				replaced = *event.ReplacedUnusedNativeSessionID
			}
			opened = append(opened, acpTestsOpened{event.NativeSessionID, event.Resumed, replaced})
		case core.RuntimeEventSessionConfigured:
			if sendPrompt && len(opened) == 2 {
				done = true
				break
			}
			if !sendPrompt && len(opened) == 1 {
				if err := os.WriteFile(exitRequested, nil, 0o644); err != nil {
					t.Fatal(err)
				}
			} else if !promptSent {
				requests <- acpTestsPromptRequest("prompt-1", "do work")
				promptSent = true
			}
		case core.RuntimeEventPromptFinished:
			if event.RequestID == "prompt-1" && !sendPrompt {
				if event.StopReason != "EndTurn" {
					t.Fatalf("stop reason %q", event.StopReason)
				}
				done = true
			}
		case core.RuntimeEventStopped:
			runner.waitBriefly(ioTimeout)
			t.Fatalf("runtime stopped instead of recovering: %s", runner.describe())
		}
	}
	wantOpened := []acpTestsOpened{{"thread-1", false, "unused"}, {"thread-2", false, "unused"}}
	wantRestart := "The agent stopped before its first prompt; restarting with a new empty thread."
	begin, commit := fmt.Sprintf("%T", core.NativeAgentEventReplayBegin{}), fmt.Sprintf("%T", core.NativeAgentEventReplayCommit{})
	wantReplay := []string{begin, commit, begin, commit}
	if sendPrompt {
		wantOpened = []acpTestsOpened{{"original", false, ""}, {"original", true, ""}}
		wantRestart = "ACP bridge exited; reloading the native session"
		wantReplay = []string{begin, commit}
	}
	if !slices.Equal(opened, wantOpened) {
		t.Fatalf("opened %+v, want %+v", opened, wantOpened)
	}
	if !slices.Equal(restartMessages, []string{wantRestart}) {
		t.Fatalf("restart messages %q, want %q", restartMessages, wantRestart)
	}
	if !slices.Equal(replay, wantReplay) {
		t.Fatalf("replay %q, want %q", replay, wantReplay)
	}
	close(requests)
	if _, err := runner.wait(t, 5*time.Second, "the runtime ends with its requests"); err != nil {
		t.Fatal(err)
	}
}

// A replacement bridge is a fresh harness process, and Codex builds the
// resumed thread's MCP set out of the `session/resume` request alone, so the
// delegation server has to be stated again at every launch. Without this the
// session's `mcp__mj_agents__*` tools disappeared after the first daemon or
// bridge restart (#1085).
func TestARelaunchedCodexSessionKeepsDelegationAndHistoryTools(t *testing.T) {
	temp := t.TempDir()
	marker := filepath.Join(temp, "second-bridge")
	opens := filepath.Join(temp, "opens.txt")
	script := filepath.Join(temp, "restarting_codex.py")
	acpTestsWriteScript(t, script, strings.NewReplacer(
		"__MARKER__", acpTestsPythonPath(t, marker),
		"__OPENS__", acpTestsPythonPath(t, opens),
	).Replace(`
import json, os, sys, time
marker = __MARKER__
opens = __OPENS__

def write(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

second = os.path.exists(marker)
mode = {"id": "interaction_mode", "name": "Mode", "category": "mode",
        "type": "select", "currentValue": "agent",
        "options": [{"value": "agent", "name": "Agent"}]}
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get("method"), request.get("id")
    if ident is None:
        continue
    if method == "initialize":
        write({"jsonrpc": "2.0", "id": ident, "result": {
            "protocolVersion": 1,
            "agentCapabilities": {"loadSession": True,
                                  "sessionCapabilities": {"resume": {}}}}})
        continue
    if method in ("session/new", "session/resume", "session/load"):
        servers = request.get("params", {}).get("mcpServers")
        names = "none" if servers is None else ",".join(
            server["name"] for server in servers)
        with open(opens, "a") as log:
            log.write(method + " " + names + "\n")
        write({"jsonrpc": "2.0", "id": ident, "result": {
            "sessionId": "scripted", "configOptions": [mode],
            "modes": {"currentModeId": "agent",
                      "availableModes": [{"id": "agent", "name": "Agent"}]}}})
        continue
    if method == "session/set_config_option":
        write({"jsonrpc": "2.0", "id": ident, "result": {"configOptions": [mode]}})
        continue
    if method == "session/prompt":
        if not second:
            # The bridge dies on a thread that has been used, the way it does
            # when the daemon restarts underneath a live session.
            open(marker, "w").close()
            time.sleep(0.2)
            break
        write({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": "scripted",
            "update": {"sessionUpdate": "agent_message_chunk",
                       "content": {"type": "text", "text": "ok"}}}})
        write({"jsonrpc": "2.0", "id": ident, "result": {"stopReason": "end_turn"}})
        continue
    write({"jsonrpc": "2.0", "id": ident, "result": {}})
`))

	requests := make(chan CommandRequest, 1)
	events := make(chan core.RuntimeEvent, 64)
	spec := acpTestsSpec(core.HarnessKindCodex, "python3", temp)
	spec.Args = []string{script}
	spec.SubagentMCPSocket = &SubagentMcpSocket{
		Path:                 filepath.Join(temp, "subagents.sock"),
		Role:                 core.SubagentMcpRoleParent,
		Oracle:               false,
		OrchestratorsEnabled: false,
		SubtleSubagents:      true,
		ReviewTools:          false,
		ProfileRegistration:  false,
	}
	spec.ProjectMemory = &core.ProjectMemoryLaunchConfig{
		ProjectKey:      "project",
		Root:            filepath.Join(temp, "memory"),
		BaselineRoot:    filepath.Join(temp, "baseline"),
		RepositoryRoots: map[string]string{},
		MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
	}
	runner := acpTestsStartRun(spec, requests, events)
	defer runner.abort()

	var started []bool
	promptSent := false
	for done := false; !done; {
		event := acpTestsExpectEvent(t, events, runner, 10*time.Second, "ACP runtime keeps reporting", "event channel stays open")
		switch event := event.(type) {
		case core.RuntimeEventSessionStarted:
			started = append(started, event.Resumed)
		case core.RuntimeEventSessionConfigured:
			if len(started) == 2 {
				done = true
				break
			}
			if !promptSent {
				// A thread is only resumed once it has been used.
				requests <- acpTestsPromptRequest("prompt-1", "do work")
				promptSent = true
			}
		case core.RuntimeEventStopped:
			t.Fatal("the worker stopped instead of relaunching")
		}
	}
	if !slices.Equal(started, []bool{false, true}) {
		t.Fatalf("the second open is a resume: %v", started)
	}

	close(requests)
	if _, err := runner.wait(t, 10*time.Second, "closing the command channel must end the runtime"); err != nil {
		t.Fatalf("a relaunched bridge must not fail the worker: %v", err)
	}

	recorded, err := os.ReadFile(opens)
	if err != nil {
		t.Fatal(err)
	}
	if want := "session/new mj-agents,mj-memory\nsession/resume mj-agents,mj-memory\n"; string(recorded) != want {
		t.Fatalf("every launch must carry delegation and history servers: %q", recorded)
	}
}

// Hard-won: bd326e2bb: Bridge startup and recovery can exceed five seconds under parallel I/O load.
func TestDeadBridgeAfterSessionStartReloadsTheNativeSession(t *testing.T) {
	temp := t.TempDir()
	marker := filepath.Join(temp, "second-bridge")
	script := filepath.Join(temp, "dying_acp.py")
	acpTestsWriteScript(t, script, strings.ReplaceAll(`
import json, os, sys
marker = __MARKER__

def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None

def write(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

second = os.path.exists(marker)
while True:
    request = read()
    if request is None:
        break
    method = request.get("method")
    ident = request.get("id")
    if method == "initialize":
        write({"jsonrpc": "2.0", "id": ident, "result": {"protocolVersion": 1}})
    elif method in ("session/new", "session/load"):
        servers = request.get("params", {}).get("mcpServers", [])
        if method == "session/new":
            assert len(servers) == 1, request
        else:
            assert [server['name'] for server in servers] == ['mj-memory'], request
            write({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": "scripted",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "replayed old history"}
                }
            }})
        write({"jsonrpc": "2.0", "id": ident, "result": {"sessionId": "scripted"}})
        if method == "session/load":
            # Codex can finish dispatching an old tool completion after the
            # load response. Its creation belongs to pre-resume history and
            # was intentionally not injected into this connection.
            write({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": "scripted",
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "old-wait-tool",
                    "status": "completed",
                    "title": "wait"
                }
            }})
        if not second:
            open(marker, "w").close()
            import time
            time.sleep(0.2)
            break
    elif ident is not None:
        write({"jsonrpc": "2.0", "id": ident, "result": {}})
`, "__MARKER__", acpTestsPythonPath(t, marker)))

	requests := make(chan CommandRequest, 1)
	events := make(chan core.RuntimeEvent, 64)
	profile := filepath.Join(temp, "profile")
	if err := os.MkdirAll(profile, 0o755); err != nil {
		t.Fatal(err)
	}
	spec := acpTestsSpec(core.HarnessKindKimi, "python3", temp)
	spec.Args = []string{script}
	spec.Environment = map[string]string{"KIMI_CODE_HOME": profile}
	spec.ProjectMemory = &core.ProjectMemoryLaunchConfig{
		ProjectKey:      "abc",
		Root:            filepath.Join(temp, "memory"),
		BaselineRoot:    filepath.Join(temp, "baseline"),
		RepositoryRoots: map[string]string{},
		MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
	}
	runner := acpTestsStartRun(spec, requests, events)
	defer runner.abort()

	var started []bool
	sawReload := false
	capabilityUpdates := 0
	for len(started) < 2 {
		event := acpTestsExpectEvent(t, events, runner, ioTimeout, "ACP runtime keeps reporting", "event channel stays open")
		switch event := event.(type) {
		case core.RuntimeEventSessionStarted:
			if event.NativeSessionID != "scripted" {
				t.Fatalf("native session %q", event.NativeSessionID)
			}
			started = append(started, event.Resumed)
		case core.RuntimeEventHarnessRestarting:
			if !strings.Contains(event.Message, "reloading the native session") {
				t.Fatal(event.Message)
			}
			sawReload = true
		case core.RuntimeEventSessionUpdate:
			update, _ := acpTestsUpdateValue(t, event)
			if !acpTestsJSONEqual(t, update, map[string]any{"sessionUpdate": "session_info_update", "_meta": map[string]any{"mjGoalCapability": nil}}) {
				t.Fatalf("resume replay leaked into the relay: %v", update)
			}
			capabilityUpdates++
		case core.RuntimeEventStopped:
			t.Fatal("worker stopped before reloading the session")
		}
	}
	if capabilityUpdates != 2 {
		t.Fatalf("each bridge refreshes its goal controls: %d updates", capabilityUpdates)
	}
	if !sawReload {
		t.Fatal("a dead bridge after session start must reload")
	}
	if !slices.Equal(started, []bool{false, true}) {
		t.Fatalf("the second open is a resume: %v", started)
	}

	close(requests)
	if _, err := runner.wait(t, ioTimeout, "closing the command channel must end the runtime"); err != nil {
		t.Fatalf("a recovered bridge must not fail the worker: %v", err)
	}
}

// Hard-won: 410851f2d: Bridge startup is not bounded by the five-second fixture wait.
func TestAnAcknowledgedCancelKeepsTheRunningBridge(t *testing.T) {
	temp := t.TempDir()
	initializes := filepath.Join(temp, "initializes")
	script := filepath.Join(temp, "cancelled_acp.py")
	acpTestsWriteScript(t, script, strings.ReplaceAll(`
import json, sys
initializes = __INITIALIZES__

def read():
    line = sys.stdin.readline()
    return json.loads(line) if line else None

def write(payload):
    sys.stdout.write(json.dumps(payload) + "\n")
    sys.stdout.flush()

prompts = 0
while True:
    request = read()
    if request is None:
        break
    method = request.get("method")
    ident = request.get("id")
    if method == "initialize":
        with open(initializes, "a") as handle:
            handle.write("x")
        write({"jsonrpc": "2.0", "id": ident, "result": {"protocolVersion": 1}})
    elif method in ("session/new", "session/load"):
        write({"jsonrpc": "2.0", "id": ident, "result": {"sessionId": "scripted"}})
    elif method == "session/prompt":
        prompts += 1
        if prompts == 1:
            write({"jsonrpc": "2.0", "method": "session/update", "params": {
                "sessionId": "scripted", "update": {"sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "prompt-ready"}}}})
            cancellation = read()
            assert cancellation.get("method") == "session/cancel", cancellation
            write({"jsonrpc": "2.0", "id": ident, "result": {"stopReason": "cancelled"}})
        else:
            write({"jsonrpc": "2.0", "id": ident, "result": {"stopReason": "end_turn"}})
    elif ident is not None:
        write({"jsonrpc": "2.0", "id": ident, "result": {}})
`, "__INITIALIZES__", acpTestsPythonPath(t, initializes)))

	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	spec := acpTestsSpec(core.HarnessKindKimi, "python3", temp)
	spec.Args = []string{script}
	runner := acpTestsStartRun(spec, requests, events)
	defer runner.abort()

	var finished []string
	waitForFinishedPrompt := func() {
		t.Helper()
		for {
			event := acpTestsExpectEvent(t, events, runner, ioTimeout, "the ACP runtime keeps reporting", "the event channel stays open")
			switch event := event.(type) {
			case core.RuntimeEventHarnessRestarting:
				t.Fatalf("an acknowledged cancel must not restart the bridge: %s", event.Message)
			case core.RuntimeEventPromptFinished:
				finished = append(finished, event.RequestID)
				return
			}
		}
	}

	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		started, ok := event.(core.RuntimeEventSessionStarted)
		return ok && !started.Resumed
	})
	requests <- acpTestsPromptRequest("prompt-1", "go")
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		update, ok := acpTestsUpdateValue(t, event)
		return ok && acpTestsGet(update, "content", "text") == "prompt-ready"
	})
	requests <- CommandRequestCancel{RequestID: "cancel-1"}
	waitForFinishedPrompt()

	// The bridge that acknowledged the cancel serves the next prompt too.
	requests <- acpTestsPromptRequest("prompt-2", "carry on")
	waitForFinishedPrompt()
	if !slices.Equal(finished, []string{"prompt-1", "prompt-2"}) {
		t.Fatalf("finished %q", finished)
	}
	recorded, err := os.ReadFile(initializes)
	if err != nil {
		t.Fatal(err)
	}
	if string(recorded) != "x" {
		t.Fatalf("the second prompt must run on the bridge that was already open: %q", recorded)
	}

	close(requests)
	if _, err := runner.wait(t, ioTimeout, "closing the command channel ends the runtime"); err != nil {
		t.Fatalf("an acknowledged cancel keeps the runtime healthy: %v", err)
	}
}

func TestBridgeExitDuringInitializeReturnsAnActionableError(t *testing.T) {
	requests := make(chan CommandRequest, 1)
	events := make(chan core.RuntimeEvent, 16)
	spec := acpTestsSpec(core.HarnessKindKimi, "sh", acpTestsCurrentDir(t))
	spec.Args = []string{
		"-c",
		// Exit during initialize, after the request has reached the bridge.
		"IFS= read -r initialize; echo 'specific supervisor failure' >&2; exit 17",
	}
	spec.ExecutionPolicy = core.ExecutionPolicyUnconstrained

	// The runtime spawns a real `sh` and waits on its real stdout, so this
	// test keeps a real clock, and the guard below only stops a hang from
	// becoming an unbounded one: the assertions are all on the error text,
	// none on how long the call took, so it can be generous enough for a
	// machine running other builds.
	runner := acpTestsStartRun(spec, requests, events)
	defer runner.abort()
	_, err := runner.wait(t, 120*time.Second, "an exited bridge must not leave ACP initialization hanging")
	if err == nil {
		t.Fatal("an exited bridge fails the runtime")
	}
	completeError := err.Error()
	if !strings.Contains(completeError, "ACP bridge") || !strings.Contains(completeError, "exit status: 17") {
		t.Fatalf("unexpected error: %s", completeError)
	}
	if !strings.Contains(completeError, "specific supervisor failure") {
		t.Fatal(completeError)
	}

	var reported []core.RuntimeEvent
	for {
		event, ok := acpTestsTryRecv(events)
		if !ok {
			break
		}
		reported = append(reported, event)
	}
	if !slices.ContainsFunc(reported, func(event core.RuntimeEvent) bool {
		fault, ok := event.(core.RuntimeEventSessionFault)
		return ok && strings.Contains(fault.Message, "ACP runtime failed")
	}) {
		t.Fatalf("the failure is reported: %#v", reported)
	}
	if !slices.ContainsFunc(reported, func(event core.RuntimeEvent) bool {
		_, ok := event.(core.RuntimeEventStopped)
		return ok
	}) {
		t.Fatalf("the stop is reported: %#v", reported)
	}
}

// Hard-won: #1379: ENOENT from current_dir was reported as a missing bridge executable.
func TestBridgeLaunchFailureIsReportedBeforeTheRuntimeStops(t *testing.T) {
	temp := t.TempDir()
	for _, launch := range []struct{ bridge, cwd string }{
		{filepath.Join(temp, "missing-acp-bridge"), temp},
		{"sh", filepath.Join(temp, "missing-checkout")},
	} {
		requests := make(chan CommandRequest, 1)
		events := make(chan core.RuntimeEvent, 16)
		spec := acpTestsSpec(core.HarnessKindKimi, launch.bridge, launch.cwd)
		spec.ExecutionPolicy = core.ExecutionPolicyUnconstrained

		err := Run(spec, requests, events)
		if err == nil {
			t.Fatalf("launching %s in %s must fail", launch.bridge, launch.cwd)
		}
		rendered := err.Error()
		if launch.bridge == "sh" {
			if !strings.Contains(rendered, fmt.Sprintf("working directory %s does not exist", launch.cwd)) {
				t.Fatal(rendered)
			}
			if strings.Contains(rendered, "executable sh was not found") {
				t.Fatal(rendered)
			}
		} else {
			if !strings.Contains(rendered, fmt.Sprintf("executable %s was not found", launch.bridge)) {
				t.Fatal(rendered)
			}
			if !strings.Contains(rendered, "working directory "+launch.cwd) {
				t.Fatal(rendered)
			}
		}
		if fault, ok := acpTestsReceive(t, events, ioTimeout, "the failure is reported").(core.RuntimeEventSessionFault); !ok ||
			!strings.Contains(fault.Message, "ACP runtime failed") {
			t.Fatalf("the first report is the failure: %#v", fault)
		}
		if _, ok := acpTestsReceive(t, events, ioTimeout, "the stop is reported").(core.RuntimeEventStopped); !ok {
			t.Fatal("the stop follows the failure")
		}
	}
}

// missingNativeSessionScript writes a fake ACP bridge that refuses every
// reload of `missing-thread` with reloadError, the JSON-RPC error the harness
// under test really answers, and opens a fresh session for `session/new`.
// Returns the script's path.
func missingNativeSessionScript(t *testing.T, directory string, reloadError any) string {
	t.Helper()
	script := filepath.Join(directory, "missing_native_session.py")
	source := `
import json, sys

RELOAD_ERROR = json.loads(__RELOAD_ERROR__)

for line in sys.stdin:
    request = json.loads(line)
    ident = request.get("id")
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": 1, "agentCapabilities": {"loadSession": True}}
    elif method in ("session/load", "session/resume"):
        print(json.dumps({"jsonrpc": "2.0", "id": ident,
                          "error": RELOAD_ERROR}),
              flush=True)
        continue
    elif method == "session/new":
        # Both harnesses' guardian modes, so either can select its own.
        result = {"sessionId": "replacement",
                  "modes": {"currentModeId": "agent",
                            "availableModes": [{"id": "agent", "name": "Guardian"},
                                               {"id": "auto", "name": "Auto"}]}}
    elif method == "session/prompt":
        result = {"stopReason": "end_turn"}
    else:
        result = {}
    if ident is not None:
        print(json.dumps({"jsonrpc": "2.0", "id": ident, "result": result}), flush=True)
`
	// A JSON string literal is also a Python string literal, so the error
	// reaches the script exactly as the harness would send it.
	encoded, err := json.Marshal(reloadError)
	if err != nil {
		t.Fatal(err)
	}
	literal, err := json.Marshal(string(encoded))
	if err != nil {
		t.Fatal(err)
	}
	acpTestsWriteScript(t, script, strings.ReplaceAll(source, "__RELOAD_ERROR__", string(literal)))
	return script
}

// missingCodexThreadError is Codex's wrapped refusal for a thread whose
// rollout was never written.
func missingCodexThreadError() map[string]any {
	return map[string]any{
		"code":    -32603,
		"message": "Internal error",
		"data":    map[string]any{"details": "no rollout found for thread id missing-thread"},
	}
}

// missingClaudeSessionError is claude-agent-acp's ACP resource error for a
// session with no transcript on disk, as 0.99 sends it.
func missingClaudeSessionError() map[string]any {
	return map[string]any{
		"code":    -32002,
		"message": "Resource not found",
		"data":    map[string]any{"sessionId": "missing-thread"},
	}
}

func missingNativeSessionSpec(directory, script string, harness core.HarnessKind, nativeSessionHistory NativeSessionHistory) LaunchSpec {
	spec := acpTestsSpec(harness, "python3", directory)
	spec.Args = []string{script}
	spec.ResumeSession = acpTestsStringPointer("missing-thread")
	spec.NativeSessionHistory = nativeSessionHistory
	return spec
}

// assertUnusedNativeSessionIsReplaced drives the runtime against a bridge
// that cannot find `missing-thread`, with Mjolnir's state showing the native
// session was never used, and proves the queued work finishes on a
// replacement session in the same Mjolnir session.
func assertUnusedNativeSessionIsReplaced(t *testing.T, harness core.HarnessKind, reloadError any) {
	t.Helper()
	temp := t.TempDir()
	script := missingNativeSessionScript(t, temp, reloadError)
	// An installed worker upgrades a snapshot whose original opening is
	// already behind its saved frontier, before ACP attempts native resume.
	root := filepath.Join(temp, "legacy-relay")
	relay, err := DurableRelayOpen(root, "upgrade-test", "old")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := relay.RecordObservation(core.RelayObservationSessionOpened{NativeSessionID: "missing-thread"}); err != nil {
		t.Fatal(err)
	}
	statePath := filepath.Join(root, core.RelayStateFile)
	contents, err := os.ReadFile(statePath)
	if err != nil {
		t.Fatal(err)
	}
	saved, err := jsonvalue.Parse(contents)
	if err != nil {
		t.Fatal(err)
	}
	state, ok := saved.(jsonvalue.Object)
	if !ok {
		t.Fatalf("the relay state is an object: %s", contents)
	}
	state.SwapDelete("native_session_used")
	state.SwapDelete("native_session_opened_ordinal")
	rewritten, err := jsonvalue.Marshal(state)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(statePath, rewritten, 0o644); err != nil {
		t.Fatal(err)
	}
	relay, err = DurableRelayOpen(root, "upgrade-test", "new")
	if err != nil {
		t.Fatal(err)
	}
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	// Work the worker queued before the session opened must survive the
	// replacement and run on the new thread.
	requests <- acpTestsPromptRequest("queued-prompt", "do the queued work")
	runner := acpTestsStartRun(missingNativeSessionSpec(temp, script, harness, relay.NativeSessionHistory()), requests, events)
	defer runner.abort()

	var warnings []string
	var opened *acpTestsOpened
	var finished *string
	for finished == nil {
		event := acpTestsExpectEvent(t, events, runner, 10*time.Second,
			"the replacement thread must open", "the runtime must keep serving the session")
		switch event := event.(type) {
		case core.RuntimeEventWarning:
			warnings = append(warnings, event.Message)
		case core.RuntimeEventSessionStarted:
			if event.ReplacedUnusedNativeSessionID == nil || *event.ReplacedUnusedNativeSessionID != "missing-thread" {
				t.Fatalf("replaced %v", event.ReplacedUnusedNativeSessionID)
			}
			opened = &acpTestsOpened{nativeSessionID: event.NativeSessionID, resumed: event.Resumed}
		case core.RuntimeEventPromptFinished:
			id := event.RequestID
			finished = &id
		case core.RuntimeEventStopped:
			t.Fatalf("the runtime stopped instead of replacing the session: %q", warnings)
		}
	}
	if opened == nil || *opened != (acpTestsOpened{nativeSessionID: "replacement"}) {
		t.Fatalf("opened %+v", opened)
	}
	if *finished != "queued-prompt" {
		t.Fatalf("finished %q", *finished)
	}
	if !slices.ContainsFunc(warnings, func(warning string) bool {
		return strings.Contains(warning, "missing-thread") && strings.Contains(warning, "new empty session") &&
			strings.Contains(warning, harness.DisplayName())
	}) {
		t.Fatalf("the replacement must be reported: %q", warnings)
	}
	close(requests)
	if _, err := runner.wait(t, 10*time.Second, "the runtime ends with its requests"); err != nil {
		t.Fatal(err)
	}
}

// Hard-won: 710ffb643: Codex could still refuse to reload a never-prompted thread after an idle worker upgrade.
func TestAnUnusedCodexThreadIsReplacedInTheSameSessionAfterUpgrade(t *testing.T) {
	assertUnusedNativeSessionIsReplaced(t, core.HarnessKindCodex, map[string]any{
		"code":    -32603,
		"message": "Internal error",
		"data":    map[string]any{"details": "thread not loaded: missing-thread"},
	})
}

func TestAnUnusedClaudeSessionIsReplacedInTheSameSessionAfterUpgrade(t *testing.T) {
	assertUnusedNativeSessionIsReplaced(t, core.HarnessKindClaude, missingClaudeSessionError())
}

func TestAUsedCodexThreadCodexCannotFindFailsInsteadOfStartingOver(t *testing.T) {
	temp := t.TempDir()
	script := missingNativeSessionScript(t, temp, missingCodexThreadError())
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	runner := acpTestsStartRun(missingNativeSessionSpec(temp, script, core.HarnessKindCodex, NativeSessionHistoryMayHaveHistory), requests, events)
	defer runner.abort()

	_, err := runner.wait(t, 10*time.Second, "the runtime must fail rather than hang")
	if err == nil {
		t.Fatal("a used thread the harness cannot find fails the runtime")
	}
	if !strings.Contains(err.Error(), "no native history for session missing-thread") {
		t.Fatalf("unexpected failure: %v", err)
	}
	var reported []core.RuntimeEvent
	for {
		event, ok := acpTestsTryRecv(events)
		if !ok {
			break
		}
		reported = append(reported, event)
	}
	if slices.ContainsFunc(reported, func(event core.RuntimeEvent) bool {
		_, started := event.(core.RuntimeEventSessionStarted)
		return started
	}) {
		t.Fatalf("a used thread must never be replaced: %#v", reported)
	}
	close(requests)
}

// Hard-won: ad4c24b: worker-local retirement must traverse the real FIFO without serialization.
func TestRealBridgeRetirementRunsThroughWorkerFifoWithoutSerialization(t *testing.T) {
	temp := t.TempDir()
	durable, err := DurableRelayOpen(temp, "retirement-fifo", "test")
	if err != nil {
		t.Fatal(err)
	}
	relay := NewRelayCell(durable)
	spec := silentBridgeSpec(core.StallPolicy{})
	spec.Harness = core.HarnessKindCodex
	spec.Command = "python3"
	spec.Args = []string{"-u", "-c", `
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if 'method' not in request or 'id' not in request: continue
    method = request['method']
    result = {'protocolVersion':1} if method == 'initialize' else {'sessionId':'retirement-native', 'modes':{'currentModeId':'agent','availableModes':[{'id':'agent','name':'Agent'},{'id':'plan','name':'Plan'}]}} if method == 'session/new' else {}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
`}
	spec.Cwd = temp
	owner := weak.Make(relay)
	spec.TerminalOwner = &owner
	requests := make(chan CommandRequest, 1)
	close(requests)
	events := make(chan core.RuntimeEvent, 64)
	wakes := newDispatchWakeChannel()
	unusedCommands := newCommandChannel(1)
	shellEvents := make(chan core.RuntimeEvent, 1)
	shells := NewUserShellRegistry(temp, map[string]string{}, shellEvents)
	coordinatorContext, stopCoordinator := context.WithCancel(context.Background())
	defer stopCoordinator()
	coordinator := make(chan error, 1)
	go func() {
		defer func() {
			if panicked := recover(); panicked != nil {
				coordinator <- fmt.Errorf("the coordinator panicked: %v", panicked)
			}
		}()
		coordinator <- runRelayCoordinatorWithVerdict(coordinatorContext, relay, events, wakes, unusedCommands, shells, nil, nil, nil)
	}()
	runner := acpTestsStartRun(spec, requests, events)
	defer runner.abort()
	// Join both even if either fails, so the fixture leaves no runtime behind.
	_, runtimeErr := runner.wait(t, ioTimeout, "the ACP runtime ends with its requests")
	// The runtime has dropped its event sender.
	close(events)
	coordinatorErr := acpTestsReceive(t, coordinator, ioTimeout, "the coordinator ends with the runtime")
	if runtimeErr != nil {
		t.Fatal(runtimeErr)
	}
	if coordinatorErr != nil {
		t.Fatal(coordinatorErr)
	}
	state := relay.Lock()
	defer state.Unlock()
	if terminals := state.OperationalState().ActiveAgentTerminals; len(terminals) != 0 {
		t.Fatalf("the retired bridge left terminals: %#v", terminals)
	}
	recorded, err := state.EventsAfter(0, core.RelayEventGenesisDigest)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.ContainsFunc(recorded, func(event core.RelayEvent) bool {
		_, opened := event.Observation.(core.RelayObservationSessionOpened)
		return opened
	}) {
		t.Fatal("the session open reached the relay")
	}
	goruntime.KeepAlive(relay)
}

// Hard-won: cd79f2d22: canceled Tokio blocking reads still held runtime shutdown open.
func TestSessionInstructionsStalledReadDoesNotHoldRuntimeShutdown(t *testing.T) {
	temp := t.TempDir()
	memoryRoot := filepath.Join(temp, "memory")
	if err := os.MkdirAll(memoryRoot, 0o755); err != nil {
		t.Fatal(err)
	}
	index := filepath.Join(memoryRoot, "MEMORY.md")
	if err := syscall.Mkfifo(index, 0o600); err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	exited := make(chan error, 1)
	go func() {
		_, err := fileReadOffRuntime(ctx, "stalled-filesystem-test", func() (string, error) {
			// The store rejects FIFOs; here one simulates a hung regular-file
			// read inside the same off-runtime reader used for valid indexes.
			contents, err := os.ReadFile(index)
			if err != nil {
				return "", fmt.Errorf("read stalled filesystem: %w", err)
			}
			return string(contents), nil
		})
		exited <- err
	}()
	// Writer open proves the reader is inside the real filesystem call. Hold
	// it open without bytes until after the reader has returned.
	writer, err := os.OpenFile(index, os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer writer.Close()
	if err, returned := acpTestsTryRecv(exited); returned {
		t.Fatalf("FIFO read ended before cancellation: %v", err)
	}
	cancel()
	select {
	case err := <-exited:
		if err == nil {
			t.Fatal("FIFO read ended before cancellation")
		}
	case <-time.After(ioTimeout):
		t.Fatal("the runtime exits without waiting for the memory reader")
	}
}

// mod terminals: terminals run real children in real process groups, which
// only Unix has.

// answerTimeout bounds every wait, so a handler that stalls the dispatch loop
// or a child that deadlocks on a full pipe fails the test in seconds instead
// of hanging the suite.
const answerTimeout = 5 * time.Second

// clientRequestBridge answers `initialize` and `session/new`, writes the
// requests a test scripts, and republishes every answer Hel sends back.
func clientRequestBridge(t *testing.T, stream io.ReadWriteCloser, scripted <-chan map[string]any, answers chan<- map[string]any) {
	bridge := acpTestsBridge(stream)
	lines := acpTestsLines(t, bridge)
	for {
		var outgoing any
		select {
		case message, ok := <-lines:
			if !ok {
				return
			}
			method, ok := message["method"].(string)
			if !ok {
				// No method: an answer to one of the scripted requests.
				answers <- message
				continue
			}
			id := message["id"]
			switch method {
			case "initialize":
				outgoing = acpTestsResult(id, map[string]any{"protocolVersion": 1})
			case "session/new":
				outgoing = acpTestsResult(id, map[string]any{"sessionId": "scripted"})
			default:
				continue
			}
		case request, ok := <-scripted:
			if !ok {
				return
			}
			outgoing = request
		}
		if !bridge.send(outgoing) {
			return
		}
	}
}

// scriptedAgent is the agent side of a scripted connection. Answers are
// collected by id, so a test can keep one request in flight while it sends
// others.
type scriptedAgent struct {
	scripted chan<- map[string]any
	answers  <-chan map[string]any
	received map[string]map[string]any
	sent     int
}

func (a *scriptedAgent) send(method string, params any) string {
	a.sent++
	id := fmt.Sprintf("agent-%d", a.sent)
	a.scripted <- map[string]any{
		"jsonrpc": "2.0",
		"id":      id,
		"method":  method,
		"params":  params,
	}
	return id
}

func (a *scriptedAgent) answer(t *testing.T, id string) map[string]any {
	t.Helper()
	for {
		if answer, ok := a.received[id]; ok {
			delete(a.received, id)
			return answer
		}
		var answer map[string]any
		select {
		case received, ok := <-a.answers:
			if !ok {
				t.Fatal("the bridge must keep publishing answers")
			}
			answer = received
		case <-time.After(answerTimeout):
			t.Fatal("Hel must answer every terminal request instead of leaving the agent waiting")
		}
		answerID, ok := answer["id"].(string)
		if !ok {
			t.Fatal("an answer must carry the request id")
		}
		a.received[answerID] = answer
	}
}

func (a *scriptedAgent) call(t *testing.T, method string, params any) map[string]any {
	t.Helper()
	return a.answer(t, a.send(method, params))
}

// scriptedObservedEvents is the runtime events a scripted runtime reported,
// shared between its event recorder and the test (Rust's
// Arc<Mutex<Vec<RuntimeEvent>>>).
type scriptedObservedEvents struct {
	mu     sync.Mutex
	events []core.RuntimeEvent
}

type scriptedRuntime struct {
	agent    scriptedAgent
	observed *scriptedObservedEvents
	requests chan<- CommandRequest
	driver   *acpTestsDriver
	// bridge is the bridge side of the duplex stream; closing it aborts the
	// bridge.
	bridge io.Closer
	// events is closed when the event recorder has drained the runtime's
	// event channel; stopEvents aborts the recorder.
	events     <-chan struct{}
	stopEvents func()
}

// stop closes the command channel and waits for the runtime to finish,
// which is also what tears the terminals down.
func (r *scriptedRuntime) stop(t *testing.T) {
	t.Helper()
	close(r.requests)
	restart, err := r.driver.wait(t, answerTimeout, "closing the command channel must end the runtime")
	if err != nil {
		t.Fatalf("terminal work must not fail the runtime: %v", err)
	}
	if restart != nil {
		t.Fatalf("restart %#v", restart)
	}
	r.bridge.Close()
	// The recorder ends with the runtime once it has recorded every event.
	select {
	case <-r.events:
	case <-time.After(answerTimeout):
		r.stopEvents()
	}
}

func startScriptedRuntime(t *testing.T) *scriptedRuntime {
	return startScriptedRuntimeWithOwner(t, nil)
}

// startScriptedRuntimeWithOwner starts a scripted Kimi runtime whose
// terminals belong to terminalOwner; nil is no owner.
func startScriptedRuntimeWithOwner(t *testing.T, terminalOwner *weak.Pointer[RelayCell]) *scriptedRuntime {
	return startScriptedRuntimeWithPause(t, terminalOwner, nil, nil)
}

// startScriptedRuntimeWithPause is startScriptedRuntimeWithOwner whose event
// recorder, given pauseReached and pauseRelease, signals pauseReached at the
// first agent message chunk and waits for pauseRelease before recording it.
// Both nil is no pause.
func startScriptedRuntimeWithPause(t *testing.T, terminalOwner *weak.Pointer[RelayCell], pauseReached chan<- struct{}, pauseRelease <-chan struct{}) *scriptedRuntime {
	t.Helper()
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	scripted := make(chan map[string]any, 1024)
	answers := make(chan map[string]any, 1024)
	go clientRequestBridge(t, bridgeStream, scripted, answers)

	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	toolsInFlight := &core.ToolsInFlight{}
	if terminalOwner != nil {
		if owner := terminalOwner.Value(); owner != nil {
			guard := owner.Lock()
			toolsInFlight = guard.ToolsInFlight()
			guard.Unlock()
		}
	}
	spec := acpTestsSpec(core.HarnessKindKimi, "scripted", acpTestsCurrentDir(t))
	spec.ToolsInFlight = toolsInFlight
	spec.TerminalOwner = terminalOwner
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)

	// Drain events so a full channel can never be mistaken for silence, and
	// keep them so a test can read what the runtime reported.
	observed := &scriptedObservedEvents{}
	recorded := make(chan struct{})
	stop := make(chan struct{})
	var stopOnce sync.Once
	go func() {
		defer close(recorded)
		inFlight := map[string]core.RelayCommand{}
		pause := pauseReached != nil
		for {
			var event core.RuntimeEvent
			select {
			case event = <-events:
			case <-driver.finished:
				select {
				case event = <-events:
				default:
					return
				}
			case <-stop:
				return
			}
			if update, ok := event.(core.RuntimeEventSessionUpdate); ok && pause {
				var value any
				if json.Unmarshal(update.Update, &value) == nil && acpTestsGet(value, "sessionUpdate") == "agent_message_chunk" {
					pause = false
					pauseReached <- struct{}{}
					select {
					case <-pauseRelease:
					case <-stop:
						return
					}
				}
			}
			if terminalOwner != nil {
				if owner := terminalOwner.Value(); owner != nil {
					if _, err := recordRuntimeEvent(owner, inFlight, event); err != nil {
						t.Errorf("record %#v: %v", event, err)
					}
				}
			}
			observed.mu.Lock()
			observed.events = append(observed.events, event)
			observed.mu.Unlock()
		}
	}()
	return &scriptedRuntime{
		agent: scriptedAgent{
			scripted: scripted,
			answers:  answers,
			received: map[string]map[string]any{},
		},
		observed:   observed,
		requests:   requests,
		driver:     driver,
		bridge:     bridgeStream,
		events:     recorded,
		stopEvents: func() { stopOnce.Do(func() { close(stop) }) },
	}
}

func terminalParams(terminalID string) map[string]any {
	return map[string]any{"sessionId": "scripted", "terminalId": terminalID}
}

// terminalCloseReports is every close report a terminal made. Waits for the
// first, then keeps watching: a second report would arrive right behind it.
func terminalCloseReports(observed *scriptedObservedEvents, terminalID string) []core.RuntimeEvent {
	reports := func() []core.RuntimeEvent {
		observed.mu.Lock()
		defer observed.mu.Unlock()
		var closed []core.RuntimeEvent
		for _, event := range observed.events {
			if report, ok := event.(core.RuntimeEventTerminalClosed); ok && report.TerminalID == terminalID {
				closed = append(closed, event)
			}
		}
		return closed
	}
	for range 100 {
		if len(reports()) > 0 {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}
	time.Sleep(200 * time.Millisecond)
	return reports()
}

func createTerminal(t *testing.T, agent *scriptedAgent, params any) string {
	t.Helper()
	created := agent.call(t, "terminal/create", params)
	if _, ok := created["result"]; !ok {
		t.Fatalf("terminal/create must be answered with a result, not the catch-all's method-not-found error: %v", created)
	}
	result, _ := created["result"].(map[string]any)
	terminalID, ok := result["terminalId"].(string)
	if !ok {
		t.Fatalf("terminal/create must return a terminal id: %v", created)
	}
	return terminalID
}

// acpTestsRelayCell is a relay cell over a fresh durable relay in root.
func acpTestsRelayCell(t *testing.T, root, sessionID string) *RelayCell {
	t.Helper()
	relay, err := DurableRelayOpen(root, sessionID, "test")
	if err != nil {
		t.Fatal(err)
	}
	return NewRelayCell(relay)
}

// Hard-won: 1f0ee89: synchronous terminal card admission overtook an earlier ACP chunk.
func TestTerminalAdmissionPreservesQueuedAssistantOutputOrder(t *testing.T) {
	owner := acpTestsRelayCell(t, t.TempDir(), "terminal-order")
	weakOwner := weak.Make(owner)
	reached := make(chan struct{}, 1)
	release := make(chan struct{}, 1)
	runner := startScriptedRuntimeWithPause(t, &weakOwner, reached, release)
	runner.agent.scripted <- acpTestsUpdate("scripted", map[string]any{
		"sessionUpdate": "agent_message_chunk", "content": map[string]any{"type": "text", "text": "Introducing the terminal"},
	})
	acpTestsReceive(t, reached, answerTimeout, "the introduction reaches the recorder")
	terminal := createTerminal(t, &runner.agent, map[string]any{
		"sessionId": "scripted", "command": "/bin/sh", "args": []any{"-c", "sleep 600"},
	})
	guard := owner.Lock()
	cut := guard.ReserveLiveBaseline()
	guard.Unlock()
	if _, ready := cut.TryRecv(); ready {
		t.Fatal("inventory blocks the cut before the FIFO is drained")
	}
	release <- struct{}{}
	runner.agent.call(t, "terminal/kill", terminalParams(terminal))
	if result := acpTestsReceive(t, cut.C(), answerTimeout, "the cut follows the drained FIFO"); result.Err != nil {
		t.Fatal(result.Err)
	}
	guard = owner.Lock()
	events, err := guard.EventsAfter(0, core.RelayEventGenesisDigest)
	guard.Unlock()
	if err != nil {
		t.Fatal(err)
	}
	chunk := slices.IndexFunc(events, func(event core.RelayEvent) bool {
		update, ok := event.Observation.(core.RelayObservationSessionUpdate)
		return ok && update.Update.AgentMessageChunk != nil
	})
	card := slices.IndexFunc(events, func(event core.RelayEvent) bool {
		update, ok := event.Observation.(core.RelayObservationSessionUpdate)
		return ok && update.Update.ToolCall != nil
	})
	if chunk < 0 || card < 0 {
		t.Fatalf("the relay recorded the chunk at %d and the card at %d", chunk, card)
	}
	if chunk >= card {
		t.Fatal("a later terminal card cannot precede the queued introduction")
	}
	runner.stop(t)
	goruntime.KeepAlive(owner)
}

// Hard-won: 9a72411: a failed synthetic terminal opened an idle Claude turn with no result.
func TestTerminalSpawnFailureReleasesItsCutRegistration(t *testing.T) {
	root := t.TempDir()
	owner := acpTestsRelayCell(t, root, "terminal-adoption")
	guard := owner.Lock()
	guard.SetTurnVerdictHarness(core.HarnessKindClaude)
	guard.Unlock()
	weakOwner := weak.Make(owner)
	runner := startScriptedRuntimeWithOwner(t, &weakOwner)
	terminal := createTerminal(t, &runner.agent, map[string]any{
		"sessionId": "scripted", "command": "/bin/sh", "args": []any{"-c", "sleep 600"},
	})
	guard = owner.Lock()
	cut := guard.ReserveLiveBaseline()
	guard.Unlock()
	if _, ready := cut.TryRecv(); ready {
		t.Fatal("the live terminal holds the cut")
	}
	failed := runner.agent.call(t, "terminal/create", map[string]any{
		"sessionId": "scripted", "command": "/bin/sh", "args": []any{"-c", "exit 0"},
		"cwd": filepath.Join(root, "missing-directory"),
	})
	if _, ok := failed["error"]; !ok {
		t.Fatalf("a spawn in a missing directory fails: %v", failed)
	}
	activeTerminals := func() int {
		guard := owner.Lock()
		defer guard.Unlock()
		return len(guard.OperationalState().ActiveAgentTerminals)
	}
	deadline := time.Now().Add(answerTimeout)
	for activeTerminals() != 1 {
		if time.Now().After(deadline) {
			t.Fatalf("active terminals %d, want the live one", activeTerminals())
		}
		goruntime.Gosched()
	}
	if active := activeTerminals(); active != 1 {
		t.Fatalf("active terminals %d", active)
	}
	if _, ready := cut.TryRecv(); ready {
		t.Fatal("the failed spawn released the live terminal's hold on the cut")
	}
	stopped := runner.agent.call(t, "terminal/kill", terminalParams(terminal))
	if _, ok := stopped["result"]; !ok {
		t.Fatalf("the live terminal is killed: %v", stopped)
	}
	result := acpTestsReceive(t, cut.C(), answerTimeout, "the cut follows the last terminal")
	if result.Err != nil {
		t.Fatal(result.Err)
	}
	if terminals := result.Baseline.State.ActiveAgentTerminals; len(terminals) != 0 {
		t.Fatalf("the cut holds terminals: %#v", terminals)
	}
	guard = owner.Lock()
	idle := guard.ToolsInFlight().IsEmpty()
	guard.Unlock()
	if !idle {
		t.Fatal("no tool call is left in flight")
	}
	runner.stop(t)
	goruntime.KeepAlive(owner)
}

func TestADisappearedTerminalOwnerRefusesSpawn(t *testing.T) {
	root := t.TempDir()
	owner := acpTestsRelayCell(t, root, "terminal-adoption")
	weakOwner := weak.Make(owner)
	owner = nil
	// Go reclaims the cell only at a collection; Rust's drop is immediate.
	for range 10 {
		if weakOwner.Value() == nil {
			break
		}
		goruntime.GC()
	}
	if weakOwner.Value() != nil {
		t.Fatal("the dropped owner is collected")
	}
	runner := startScriptedRuntimeWithOwner(t, &weakOwner)
	marker := filepath.Join(root, "must-not-spawn")
	failed := runner.agent.call(t, "terminal/create", map[string]any{
		"sessionId": "scripted", "command": "/bin/sh", "args": []any{"-c", "touch " + core.PosixQuote(marker)},
	})
	if _, ok := failed["error"]; !ok {
		t.Fatalf("a terminal without an owner is refused: %v", failed)
	}
	encoded, err := json.Marshal(failed)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(encoded), "relay owner is gone") {
		t.Fatalf("the refusal names the gone owner: %s", encoded)
	}
	if _, err := os.Stat(marker); err == nil {
		t.Fatal("the refused terminal ran")
	}
	runner.stop(t)
}

func TestTerminalOutputKeepsTheLastBytesWhenAChildExceedsTheLimit(t *testing.T) {
	runner := startScriptedRuntime(t)
	// 512 KiB is far past the 64 KiB pipe buffer: a supervisor that did not
	// drain the pipes while the child ran would block it forever, and the
	// answer timeouts would report that as a failure.
	script := "data=0123456789abcdef; " +
		"while [ ${#data} -lt 524288 ]; do data=\"$data$data\"; done; " +
		"printf '%s' \"$data\"; printf 'TAIL-MARKER'"
	const limit = 8 * 1024
	terminalID := createTerminal(t, &runner.agent, map[string]any{
		"sessionId":       "scripted",
		"command":         "/bin/sh",
		"args":            []any{"-c", script},
		"outputByteLimit": limit,
	})

	exited := runner.agent.call(t, "terminal/wait_for_exit", terminalParams(terminalID))
	if code := acpTestsGet(exited, "result", "exitCode"); code != float64(0) {
		t.Fatalf("exit %v", exited)
	}

	output := runner.agent.call(t, "terminal/output", terminalParams(terminalID))
	text, ok := acpTestsString(acpTestsGet(output, "result", "output"))
	if !ok {
		t.Fatalf("terminal/output must serve text: %v", output)
	}
	if len(text) > limit {
		t.Fatalf("served %d bytes for a %d byte limit", len(text), limit)
	}
	if !strings.HasSuffix(text, "TAIL-MARKER") {
		t.Fatalf("the retained output must be the tail, ended with %q", text[max(0, len(text)-32):])
	}
	if acpTestsGet(output, "result", "truncated") != true {
		t.Fatalf("the output is marked truncated: %v", output)
	}

	runner.stop(t)
}

// acpTestsAwaitRunning polls a terminal's output until it reads "running".
func acpTestsAwaitRunning(t *testing.T, runner *scriptedRuntime, terminalID string) string {
	t.Helper()
	running := ""
	for range 100 {
		polled := runner.agent.call(t, "terminal/output", terminalParams(terminalID))
		running, _ = acpTestsString(acpTestsGet(polled, "result", "output"))
		if running == "running" {
			break
		}
		time.Sleep(20 * time.Millisecond)
	}
	return running
}

func TestTerminalKillReportsTheSignalAndKeepsOutputReadable(t *testing.T) {
	runner := startScriptedRuntime(t)
	terminalID := createTerminal(t, &runner.agent, map[string]any{
		"sessionId": "scripted",
		"command":   "printf running; exec sleep 300",
		"args":      []any{},
	})

	// The wait stays outstanding while the terminal runs: an inline wait
	// would stall the dispatch loop and nothing below could be answered.
	waiting := runner.agent.send("terminal/wait_for_exit", terminalParams(terminalID))
	if running := acpTestsAwaitRunning(t, runner, terminalID); running != "running" {
		t.Fatalf("a live terminal must serve its output: %q", running)
	}

	killed := runner.agent.call(t, "terminal/kill", terminalParams(terminalID))
	if _, ok := killed["result"]; !ok {
		t.Fatalf("kill %v", killed)
	}

	exited := runner.agent.answer(t, waiting)
	if acpTestsGet(exited, "result", "signal") != "SIGKILL" {
		t.Fatalf("exit %v", exited)
	}
	if acpTestsHas(exited, "result", "exitCode") {
		t.Fatalf("a killed terminal has no exit code: %v", exited)
	}

	// A kill does not release the terminal.
	after := runner.agent.call(t, "terminal/output", terminalParams(terminalID))
	if acpTestsGet(after, "result", "output") != "running" || acpTestsGet(after, "result", "exitStatus", "signal") != "SIGKILL" {
		t.Fatalf("output after the kill %v", after)
	}

	released := runner.agent.call(t, "terminal/release", terminalParams(terminalID))
	if _, ok := released["result"]; !ok {
		t.Fatalf("release %v", released)
	}

	// The transcript gets one report per terminal, from whichever of kill,
	// release, or teardown reaped the child.
	observed := runner.observed
	runner.stop(t)
	reports := terminalCloseReports(observed, terminalID)
	if len(reports) != 1 {
		t.Fatalf("a killed and released terminal must report its close once: %#v", reports)
	}
	closed, ok := reports[0].(core.RuntimeEventTerminalClosed)
	if !ok {
		t.Fatalf("expected a terminal close report: %#v", reports)
	}
	if closed.Output != "running" {
		t.Fatalf("output %q", closed.Output)
	}
	if closed.Signal == nil || *closed.Signal != "SIGKILL" {
		t.Fatalf("signal %v", closed.Signal)
	}
}

func TestCancelKillsLiveClientTerminals(t *testing.T) {
	runner := startScriptedRuntime(t)
	terminalID := createTerminal(t, &runner.agent, map[string]any{
		"sessionId": "scripted",
		"command":   "printf running; exec sleep 300",
		"args":      []any{},
	})

	waiting := runner.agent.send("terminal/wait_for_exit", terminalParams(terminalID))
	if running := acpTestsAwaitRunning(t, runner, terminalID); running != "running" {
		t.Fatalf("a live terminal must serve its output: %q", running)
	}

	runner.requests <- CommandRequestCancel{RequestID: "cancel-terminals"}

	// The answer's own bound is the cancel's: it must kill the terminal so
	// wait_for_exit can finish.
	exited := runner.agent.answer(t, waiting)
	if acpTestsGet(exited, "result", "signal") != "SIGKILL" {
		t.Fatalf("exit %v", exited)
	}

	runner.stop(t)
}

// acpTestsProcessIsGone reports a process still visible but already dead — a
// zombie waiting for its parent — as gone; the parent died with it. Rust's
// process_is_gone; prefixed because user_shell's tests have their own.
func acpTestsProcessIsGone(pid int) bool {
	// Signal 0 only probes whether the process exists.
	if err := syscall.Kill(pid, 0); err != nil {
		return true
	}
	stat, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
	if err != nil {
		return false
	}
	text := string(stat)
	rest := text[strings.LastIndex(text, ")")+1:]
	return strings.HasPrefix(strings.TrimLeftFunc(rest, unicode.IsSpace), "Z")
}

// startTerminalWithAGrandchild starts a shell that keeps a grandchild alive
// and publishes both pids, so a test can prove a kill reached the whole
// process group rather than only the shell Hel spawned.
func startTerminalWithAGrandchild(t *testing.T, runner *scriptedRuntime, pidsPath string) []int {
	t.Helper()
	script := fmt.Sprintf("sleep 300 & printf '%%s %%s' \"$$\" \"$!\" > '%s'; wait", pidsPath)
	createTerminal(t, &runner.agent, map[string]any{
		"sessionId": "scripted",
		"command":   "/bin/sh",
		"args":      []any{"-c", script},
	})

	var pids []int
	for range 250 {
		if recorded, err := os.ReadFile(pidsPath); err == nil {
			pids = nil
			for _, field := range strings.Fields(string(recorded)) {
				if pid, err := strconv.Atoi(field); err == nil {
					pids = append(pids, pid)
				}
			}
			if len(pids) == 2 {
				break
			}
		}
		time.Sleep(20 * time.Millisecond)
	}
	if len(pids) != 2 {
		t.Fatalf("the terminal must report both of its pids: %v", pids)
	}
	return pids
}

func assertProcessesAreGone(t *testing.T, pids []int) {
	t.Helper()
	for _, pid := range pids {
		gone := false
		for range 250 {
			if acpTestsProcessIsGone(pid) {
				gone = true
				break
			}
			time.Sleep(20 * time.Millisecond)
		}
		if !gone {
			t.Fatalf("process %d survived the runtime that started it", pid)
		}
	}
}

func TestRuntimeTeardownKillsTerminalProcessGroups(t *testing.T) {
	pidsPath := filepath.Join(t.TempDir(), "pids")
	runner := startScriptedRuntime(t)
	// Nothing killed or released this terminal: teardown owns it.
	pids := startTerminalWithAGrandchild(t, runner, pidsPath)

	runner.stop(t)

	assertProcessesAreGone(t, pids)
}

func TestDroppingTheConnectionKillsTerminalProcessGroups(t *testing.T) {
	pidsPath := filepath.Join(t.TempDir(), "pids")
	runner := startScriptedRuntime(t)
	pids := startTerminalWithAGrandchild(t, runner, pidsPath)

	// A bridge that dies mid-session leaves the runtime dropping the whole
	// connection rather than ending its command loop, so orderly teardown
	// never runs and the terminals still must not survive.
	runner.driver.abort()

	assertProcessesAreGone(t, pids)
	runner.bridge.Close()
	runner.stopEvents()
}

// acpTestsAwaitResolution waits for a oneshot command resolution, failing
// the test when none arrives in time.
func acpTestsAwaitResolution(t *testing.T, resolution <-chan error, what string) error {
	t.Helper()
	select {
	case outcome := <-resolution:
		return outcome
	case <-time.After(30 * time.Second):
		t.Fatal(what)
		return nil
	}
}

// acpTestsAwaitConfigured reads events until the session is configured.
func acpTestsAwaitConfigured(t *testing.T, events <-chan core.RuntimeEvent, driver *acpTestsDriver) {
	t.Helper()
	var seen []string
	for {
		event, closed, timedOut := acpTestsNext(events, driver, 30*time.Second)
		if closed || timedOut {
			t.Fatalf("initial bridge should open and configure; seen=%v", seen)
		}
		if _, ok := event.(core.RuntimeEventSessionConfigured); ok {
			return
		}
		seen = append(seen, fmt.Sprintf("%#v", event))
	}
}

func TestReloadLaunchConfigPropagatesReplacementBridgeStartupFailure(t *testing.T) {
	_ = harnessinstall.InitializeFromParent()
	temp := t.TempDir()
	spec := silentBridgeSpec(core.StallPolicy{})
	spec.Cwd = temp
	spec.Harness = core.HarnessKindCodex
	spec.Command = "python3"
	spec.Args = []string{
		"-u",
		"-c",
		`
import json, sys
for line in sys.stdin:
    req = json.loads(line)
    if "method" not in req or "id" not in req:
        continue
    m = req["method"]
    if m == "initialize":
        res = {"protocolVersion": 1}
    elif m == "session/new":
        res = {"sessionId": "ses-initial", "modes": {"currentModeId": "agent", "availableModes": [{"id": "agent", "name": "Agent"}, {"id": "plan", "name": "Plan"}]}}
    else:
        res = {}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": res}), flush=True)
`,
	}

	requests := make(chan CommandRequest, 8)
	events := make(chan core.RuntimeEvent, 32)
	runtime := acpTestsStartRun(spec, requests, events)

	acpTestsAwaitConfigured(t, events, runtime)

	badLaunch := launchConfig(temp)
	badLaunch.Cwd = filepath.Join(temp, "nonexistent-reload-cwd")
	resolution := make(chan error, 1)
	requests <- CommandRequestReloadLaunchConfig{
		Environment:           badLaunch.Environment,
		LaunchConfig:          &badLaunch,
		AdditionalDirectories: []string{},
		Resolved:              resolution,
	}

	err := acpTestsAwaitResolution(t, resolution, "reload reply channel should carry the startup failure")
	if err == nil {
		t.Fatal("replacement bridge startup failure must not ack Ok")
	}
	if !strings.Contains(err.Error(), "does not exist") && !strings.Contains(err.Error(), "nonexistent-reload-cwd") {
		t.Fatalf("unexpected error: %v", err)
	}
	runtime.abort()
	runtime.waitBriefly(10 * time.Second)
}

func TestReloadLaunchConfigSpawnsUpdatedCommandArgsAndWritesSupervisorSpec(t *testing.T) {
	_ = harnessinstall.InitializeFromParent()
	temp := t.TempDir()
	bridgeScript := func(sessionID string) string {
		return fmt.Sprintf(`
import json, sys
for line in sys.stdin:
    req = json.loads(line)
    if "method" not in req or "id" not in req:
        continue
    m = req["method"]
    if m == "initialize":
        res = {"protocolVersion": 1}
    elif m == "session/new":
        res = {"sessionId": "%s", "modes": {"currentModeId": "agent", "availableModes": [{"id": "agent", "name": "Agent"}, {"id": "default", "name": "Default"}, {"id": "bypassPermissions", "name": "Bypass"}]}}
    else:
        res = {}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": res}), flush=True)
`, sessionID)
	}
	spec := silentBridgeSpec(core.StallPolicy{})
	spec.Cwd = temp
	spec.Harness = core.HarnessKindCodex
	spec.Command = "python3"
	spec.Args = []string{"-u", "-c", bridgeScript("ses-initial")}

	requests := make(chan CommandRequest, 8)
	events := make(chan core.RuntimeEvent, 32)
	runtime := acpTestsStartRun(spec, requests, events)

	acpTestsAwaitConfigured(t, events, runtime)

	supervisorPath := filepath.Join(temp, "acp-supervisor.json")
	nextLaunch := launchConfig(temp)
	nextLaunch.Cwd = temp
	nextLaunch.Harness = core.HarnessKindClaude
	nextCommand := "python3"
	nextArgs := []string{"-u", "-c", bridgeScript("ses-reloaded")}
	nextSupervisor := AcpSupervisorSpecFrom(&nextLaunch)
	nextSupervisor.Command = nextCommand
	nextSupervisor.Args = slices.Clone(nextArgs)
	resolution := make(chan error, 1)
	requests <- CommandRequestReloadLaunchConfig{
		Environment:           nextLaunch.Environment,
		Command:               &nextCommand,
		Args:                  &nextArgs,
		LaunchConfig:          &nextLaunch,
		AdditionalDirectories: []string{},
		SupervisorSpec:        &ReloadSupervisorSpec{Path: supervisorPath, Spec: nextSupervisor},
		Resolved:              resolution,
	}

	if err := acpTestsAwaitResolution(t, resolution, "reload reply channel should resolve"); err != nil {
		t.Fatalf("replacement bridge should open cleanly: %v", err)
	}

	writtenSpec, err := os.ReadFile(supervisorPath)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(writtenSpec), "ses-reloaded") {
		t.Fatalf("expected updated supervisor spec on disk, got: %s", writtenSpec)
	}
	// Rust's current-thread runtime has emitted the replacement's session
	// events by the time the test reads them; here they are awaited.
	sawReloaded := false
	for !sawReloaded {
		event, closed, timedOut := acpTestsNext(events, runtime, 10*time.Second)
		if closed || timedOut {
			break
		}
		sawReloaded = strings.Contains(fmt.Sprintf("%#v", event), "ses-reloaded")
	}
	if !sawReloaded {
		t.Fatal("replacement bridge should have opened with the new command/args (ses-reloaded)")
	}
	runtime.abort()
	runtime.waitBriefly(10 * time.Second)
}

func TestReloadLaunchConfigClearsStaleAcceptedConfigSelectors(t *testing.T) {
	spec := silentBridgeSpec(core.StallPolicy{})
	spec.Harness = core.HarnessKindCodex
	oldModel := "old-codex-model"
	spec.InitialModel = &oldModel
	setAccepted := func(model, effort string) {
		accepted, unlock := spec.AcceptedConfig.Lock()
		*accepted = core.AcceptedSessionConfig{Model: &model, Effort: &effort}
		unlock()
	}
	acceptedEffort := func() *string {
		accepted, unlock := spec.AcceptedConfig.Lock()
		defer unlock()
		return accepted.Effort
	}
	setAccepted("accepted-codex-model", "high")

	// Same harness with a new initial_model clears stale accepted model so
	// StartupModel selects the new Move model, while preserving effort.
	sameHarness := launchConfig("/profile")
	sameHarness.Harness = core.HarnessKindCodex
	newModel := "new-codex-model"
	sameHarness.InitialModel = &newModel
	spec.applyReloadAcceptedConfig(&sameHarness)
	spec.InitialModel = sameHarness.InitialModel
	if model := spec.StartupModel(); model == nil || *model != "new-codex-model" {
		t.Fatalf("startup model %v", model)
	}
	if effort := acceptedEffort(); effort == nil || *effort != "high" {
		t.Fatalf("effort %v", effort)
	}

	// Harness switch clears accepted_config completely (both model and
	// effort).
	setAccepted("accepted-codex-model", "xhigh")
	crossHarness := launchConfig("/profile")
	crossHarness.Harness = core.HarnessKindClaude
	claudeModel := "claude-sonnet"
	crossHarness.InitialModel = &claudeModel
	spec.applyReloadAcceptedConfig(&crossHarness)
	spec.Harness = crossHarness.Harness
	spec.InitialModel = crossHarness.InitialModel
	if model := spec.StartupModel(); model == nil || *model != "claude-sonnet" {
		t.Fatalf("startup model %v", model)
	}
	if effort := acceptedEffort(); effort != nil {
		t.Fatalf("effort %v", *effort)
	}
}

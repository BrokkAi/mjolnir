// Port of mj-worker/src/acp/tests.rs: its #[cfg(unix)] items, including
// mod terminals.

//go:build unix

package worker

import (
	"fmt"
	"io"
	"os"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"
	"unicode"
	"weak"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func codexBridgeRestartPreservesOnlyUsedThreads(t *testing.T, sendPrompt bool) {
	panic("unported: mj-worker/src/acp/tests.rs codex_bridge_restart_preserves_only_used_threads")
}

// missingNativeSessionScript writes a fake ACP bridge that refuses every
// reload of `missing-thread` with reloadError, the JSON-RPC error the harness
// under test really answers, and opens a fresh session for `session/new`.
// Returns the script's path.
func missingNativeSessionScript(t *testing.T, directory string, reloadError any) string {
	panic("unported: mj-worker/src/acp/tests.rs missing_native_session_script")
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
	panic("unported: mj-worker/src/acp/tests.rs missing_native_session_spec")
}

// assertUnusedNativeSessionIsReplaced drives the runtime against a bridge
// that cannot find `missing-thread`, with Mjolnir's state showing the native
// session was never used, and proves the queued work finishes on a
// replacement session in the same Mjolnir session.
func assertUnusedNativeSessionIsReplaced(t *testing.T, harness core.HarnessKind, reloadError any) {
	panic("unported: mj-worker/src/acp/tests.rs assert_unused_native_session_is_replaced")
}

// mod terminals

// answerTimeout bounds every wait, so a handler that stalls the dispatch loop
// or a child that deadlocks on a full pipe fails the test in seconds instead
// of hanging the suite.
const answerTimeout = 5 * time.Second

// clientRequestBridge answers `initialize` and `session/new`, writes the
// requests a test scripts, and republishes every answer Hel sends back.
func clientRequestBridge(t *testing.T, stream io.ReadWriteCloser, scripted <-chan map[string]any, answers chan<- map[string]any) {
	panic("unported: mj-worker/src/acp/tests.rs terminals::client_request_bridge")
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

// scriptedDriveResult is what the runtime's drive returned (Rust's
// Result<Option<SessionRestart>>); a nil restart is none.
type scriptedDriveResult struct {
	restart SessionRestart
	err     error
}

type scriptedRuntime struct {
	agent    scriptedAgent
	observed *scriptedObservedEvents
	requests chan<- CommandRequest
	// driver delivers drive's result once the runtime ends.
	driver <-chan scriptedDriveResult
	// bridge is the bridge side of the duplex stream; closing it aborts the
	// bridge.
	bridge io.Closer
	// events is closed when the event recorder has drained the runtime's
	// event channel.
	events <-chan struct{}
}

// stop closes the command channel and waits for the runtime to finish,
// which is also what tears the terminals down.
func (r *scriptedRuntime) stop(t *testing.T) {
	panic("unported: mj-worker/src/acp/tests.rs terminals::ScriptedRuntime::stop")
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
	panic("unported: mj-worker/src/acp/tests.rs terminals::start_scripted_runtime_with_pause")
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
func startTerminalWithAGrandchild(t *testing.T, runtime *scriptedRuntime, pidsPath string) []int {
	panic("unported: mj-worker/src/acp/tests.rs terminals::start_terminal_with_a_grandchild")
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

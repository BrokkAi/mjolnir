// Port of mj-worker/src/acp/tests.rs: the fixtures its tests share.

// Tests of the ACP runtime: fake bridges speak JSON-RPC lines over an
// in-memory duplex stream (or a Python script over a real pipe) and the tests
// assert what the runtime reports and what the bridge received. The tests are
// in acp_tests_*_test.go; the #[cfg(unix)] items, including mod terminals,
// are in acp_unix_test.go.
//
// Go shapes used throughout:
//   - a bridge's tokio::io::DuplexStream is an io.ReadWriteCloser
//     (acpTestsDuplex); closing it is how a test aborts the bridge;
//   - serde_json::Value is the value encoding/json decodes into an any
//     (map[string]any, []any, float64, ...);
//   - an unbounded mpsc sender or a oneshot sender is a send-only channel the
//     test makes with enough buffer that the bridge never blocks on it;
//   - a spawned runtime is an acpTestsDriver: its result, and a finished
//     channel that stands for the event sender it dropped when it ended.

package worker

import (
	"bufio"
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// acpTestsPipe is one direction of an in-memory duplex stream. Writes wait
// while capacity bytes are buffered, as tokio's DuplexStream does.
type acpTestsPipe struct {
	mu       sync.Mutex
	cond     *sync.Cond
	buffer   []byte
	capacity int
	// writerClosed: the writing end is gone, so reads drain and then end.
	writerClosed bool
	// readerClosed: the reading end is gone, so writes fail.
	readerClosed bool
}

func newAcpTestsPipe(capacity int) *acpTestsPipe {
	pipe := &acpTestsPipe{capacity: capacity}
	pipe.cond = sync.NewCond(&pipe.mu)
	return pipe
}

func (p *acpTestsPipe) read(b []byte) (int, error) {
	if len(b) == 0 {
		return 0, nil
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	for len(p.buffer) == 0 {
		if p.writerClosed || p.readerClosed {
			return 0, io.EOF
		}
		p.cond.Wait()
	}
	n := copy(b, p.buffer)
	p.buffer = p.buffer[n:]
	p.cond.Broadcast()
	return n, nil
}

func (p *acpTestsPipe) write(b []byte) (int, error) {
	p.mu.Lock()
	defer p.mu.Unlock()
	written := 0
	for written < len(b) {
		if p.readerClosed || p.writerClosed {
			return written, io.ErrClosedPipe
		}
		room := p.capacity - len(p.buffer)
		if room <= 0 {
			p.cond.Wait()
			continue
		}
		n := min(room, len(b)-written)
		p.buffer = append(p.buffer, b[written:written+n]...)
		written += n
		p.cond.Broadcast()
	}
	return written, nil
}

func (p *acpTestsPipe) closeWriter() {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.writerClosed = true
	p.cond.Broadcast()
}

func (p *acpTestsPipe) closeReader() {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.readerClosed = true
	p.buffer = nil
	p.cond.Broadcast()
}

// acpTestsDuplexEnd is one end of tokio::io::duplex. Closing it is dropping
// the stream: the peer reads what was written and then the end of input, and
// its writes fail.
type acpTestsDuplexEnd struct {
	incoming *acpTestsPipe
	outgoing *acpTestsPipe
}

func (e *acpTestsDuplexEnd) Read(b []byte) (int, error) { return e.incoming.read(b) }

func (e *acpTestsDuplexEnd) Write(b []byte) (int, error) { return e.outgoing.write(b) }

func (e *acpTestsDuplexEnd) Close() error {
	e.outgoing.closeWriter()
	e.incoming.closeReader()
	return nil
}

// acpTestsDuplex is tokio::io::duplex(capacity): the client's end and the
// bridge's end of one in-memory stream.
func acpTestsDuplex(capacity int) (*acpTestsDuplexEnd, *acpTestsDuplexEnd) {
	toBridge := newAcpTestsPipe(capacity)
	toClient := newAcpTestsPipe(capacity)
	return &acpTestsDuplexEnd{incoming: toClient, outgoing: toBridge},
		&acpTestsDuplexEnd{incoming: toBridge, outgoing: toClient}
}

// acpTestsTransport is the client end as Rust's
// ByteStreams::new(client_write.compat_write(), client_read.compat()).
func acpTestsTransport(client io.ReadWriteCloser) ByteStreams {
	return NewByteStreams(client, client)
}

// acpTestsBridgeIO is a fake bridge's side of its stream: JSON-RPC lines in
// (Rust's BufReader::lines over the read half) and out (write_all on the
// write half).
type acpTestsBridgeIO struct {
	lines  *bufio.Reader
	output io.Writer
}

func acpTestsBridge(stream io.ReadWriter) *acpTestsBridgeIO {
	return &acpTestsBridgeIO{lines: bufio.NewReader(stream), output: stream}
}

// line is the next line without its newline; false at the end of input.
func (b *acpTestsBridgeIO) line() (string, bool) {
	line, err := b.lines.ReadString('\n')
	if err != nil && line == "" {
		return "", false
	}
	return strings.TrimSuffix(strings.TrimSuffix(line, "\n"), "\r"), true
}

// next is the next message the client sent; false at the end of input.
func (b *acpTestsBridgeIO) next(t *testing.T) (map[string]any, bool) {
	line, ok := b.line()
	if !ok {
		return nil, false
	}
	message, err := acpTestsDecodeObject([]byte(line))
	if err != nil {
		t.Errorf("fake adapter input is JSON-RPC: %v: %s", err, line)
		return nil, false
	}
	return message, true
}

// send writes message as one JSON-RPC line; false once the client is gone.
func (b *acpTestsBridgeIO) send(message any) bool {
	encoded, err := json.Marshal(message)
	if err != nil {
		panic(fmt.Sprintf("encode a fake bridge message: %v", err))
	}
	_, err = b.output.Write(append(encoded, '\n'))
	return err == nil
}

// acpTestsLines reads the client's messages on their own goroutine, so a
// bridge can wait for input and for something else at once (Rust's
// tokio::select! over next_line). The channel closes at the end of input.
func acpTestsLines(t *testing.T, bridge *acpTestsBridgeIO) <-chan map[string]any {
	lines := make(chan map[string]any)
	go func() {
		defer close(lines)
		for {
			message, ok := bridge.next(t)
			if !ok {
				return
			}
			lines <- message
		}
	}()
	return lines
}

func acpTestsDecodeObject(data []byte) (map[string]any, error) {
	var message map[string]any
	if err := json.Unmarshal(data, &message); err != nil {
		return nil, err
	}
	return message, nil
}

// acpTestsResult is a JSON-RPC response.
func acpTestsResult(id, result any) map[string]any {
	return map[string]any{"jsonrpc": "2.0", "id": id, "result": result}
}

// acpTestsUpdate is a `session/update` notification.
func acpTestsUpdate(sessionID string, update any) map[string]any {
	return map[string]any{
		"jsonrpc": "2.0",
		"method":  "session/update",
		"params":  map[string]any{"sessionId": sessionID, "update": update},
	}
}

// acpTestsPlain is a value as serde_json::to_value holds it, decoded by
// encoding/json: map[string]any, []any, float64, string, bool or nil.
func acpTestsPlain(t *testing.T, value any) any {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatalf("encode %T: %v", value, err)
	}
	var plain any
	if err := json.Unmarshal(encoded, &plain); err != nil {
		t.Fatalf("decode %s: %v", encoded, err)
	}
	return plain
}

// acpTestsJSONEqual is serde_json::Value equality: object members compare
// regardless of order and every number as a float.
func acpTestsJSONEqual(t *testing.T, a, b any) bool {
	t.Helper()
	return reflect.DeepEqual(acpTestsPlain(t, a), acpTestsPlain(t, b))
}

// acpTestsGet is serde_json's `value["key"][index]...`: the value at path,
// or nil (Null) when any step is missing.
func acpTestsGet(value any, path ...any) any {
	for _, step := range path {
		switch key := step.(type) {
		case string:
			object, ok := value.(map[string]any)
			if !ok {
				return nil
			}
			value = object[key]
		case int:
			array, ok := value.([]any)
			if !ok || key < 0 || key >= len(array) {
				return nil
			}
			value = array[key]
		default:
			panic(fmt.Sprintf("a JSON path step is a key or an index, not %T", step))
		}
	}
	return value
}

// acpTestsHas is serde_json's `value.get(key).is_some()` after the path.
func acpTestsHas(value any, path ...any) bool {
	if len(path) == 0 {
		return value != nil
	}
	parent := acpTestsGet(value, path[:len(path)-1]...)
	switch key := path[len(path)-1].(type) {
	case string:
		object, ok := parent.(map[string]any)
		if !ok {
			return false
		}
		_, present := object[key]
		return present
	case int:
		array, ok := parent.([]any)
		return ok && key >= 0 && key < len(array)
	}
	return false
}

// acpTestsPointer is serde_json's Value::pointer.
func acpTestsPointer(value any, pointer string) (any, bool) {
	if pointer == "" {
		return value, true
	}
	for _, token := range strings.Split(strings.TrimPrefix(pointer, "/"), "/") {
		token = strings.ReplaceAll(strings.ReplaceAll(token, "~1", "/"), "~0", "~")
		switch current := value.(type) {
		case map[string]any:
			next, ok := current[token]
			if !ok {
				return nil, false
			}
			value = next
		case []any:
			var index int
			if _, err := fmt.Sscanf(token, "%d", &index); err != nil || index < 0 || index >= len(current) {
				return nil, false
			}
			value = current[index]
		default:
			return nil, false
		}
	}
	return value, true
}

// acpTestsString is Value::as_str: the string, or "" and false.
func acpTestsString(value any) (string, bool) {
	text, ok := value.(string)
	return text, ok
}

// acpTestsArray is Value::as_array.
func acpTestsArray(value any) []any {
	array, _ := value.([]any)
	return array
}

// acpTestsUpdateValue is the update a SessionUpdate runtime event carries, as
// a JSON value.
func acpTestsUpdateValue(t *testing.T, event core.RuntimeEvent) (any, bool) {
	t.Helper()
	update, ok := event.(core.RuntimeEventSessionUpdate)
	if !ok {
		return nil, false
	}
	var value any
	if err := json.Unmarshal(update.Update, &value); err != nil {
		t.Fatalf("a session update is JSON: %v", err)
	}
	return value, true
}

// acpTestsText is ContentBlock::Text(TextContent::new(text)).
func acpTestsText(text string) schema.ContentBlock {
	return schema.ContentBlock{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: text}}
}

// acpTestsPromptRequest is CommandRequest::Prompt with one text block.
func acpTestsPromptRequest(requestID, text string) CommandRequestPrompt {
	return CommandRequestPrompt{RequestID: requestID, Prompt: []schema.ContentBlock{acpTestsText(text)}}
}

// acpTestsDecode decodes text into a T, failing the test when it does not.
func acpTestsDecode[T any](t *testing.T, text string) T {
	t.Helper()
	var value T
	if err := json.Unmarshal([]byte(text), &value); err != nil {
		t.Fatalf("decode %T from %s: %v", value, text, err)
	}
	return value
}

// acpTestsCurrentDir is std::env::current_dir().unwrap().
func acpTestsCurrentDir(t *testing.T) string {
	t.Helper()
	directory, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	return directory
}

func acpTestsStringPointer(value string) *string { return &value }

// acpTestsSpec is the LaunchSpec the Rust tests spell out field by field:
// every shared handle as its Default creates it, no Mjolnir MCP servers, no
// resume, and classification disabled. Tests change the fields they care
// about.
func acpTestsSpec(harness core.HarnessKind, command, cwd string) LaunchSpec {
	return LaunchSpec{
		LoginIdentity:        &ClaudeLoginIdentity{},
		OperatorAway:         &OperatorAway{},
		GoalRecovery:         NewShared(core.GoalRecoveryContext{}),
		Command:              command,
		Environment:          map[string]string{},
		Cwd:                  cwd,
		SubagentPolicy:       core.SubagentPolicy{Mode: core.SubagentPolicyNative},
		NativeSessionHistory: NativeSessionHistoryLoadBeforeReplace,
		AcceptedConfig:       NewShared(core.AcceptedSessionConfig{}),
		Harness:              harness,
		ExecutionPolicy:      core.ExecutionPolicyConfiguredApprovals,
		ApprovalChoices:      core.DefaultApprovalChoices(),
		ACPActivity:          core.NewAcpActivityClock(),
		StepClock:            &core.StepClock{},
		ToolsInFlight:        &core.ToolsInFlight{},
		TurnContext:          &transcript.TurnContext{},
		Verdict:              VerdictSourceDisabled{},
	}
}

// acpTestsOutcome is what a spawned runtime returned (Rust's JoinHandle
// output): drive's restart and error, or the panic it died of.
type acpTestsOutcome struct {
	restart  SessionRestart
	err      error
	panicked any
}

// acpTestsDriver is a spawned drive or run. finished closes when it returns,
// which is when Rust's runtime drops its event sender.
type acpTestsDriver struct {
	cancel   context.CancelFunc
	finished chan struct{}
	outcome  acpTestsOutcome
}

func acpTestsSpawn(body func(ctx context.Context) (SessionRestart, error)) *acpTestsDriver {
	ctx, cancel := context.WithCancel(context.Background())
	driver := &acpTestsDriver{cancel: cancel, finished: make(chan struct{})}
	go func() {
		defer close(driver.finished)
		defer func() {
			if panicked := recover(); panicked != nil {
				driver.outcome.panicked = panicked
				// A test waiting on events rather than on the runtime still
				// shows why the runtime ended.
				fmt.Fprintf(os.Stderr, "the ACP runtime task panicked: %v\n", panicked)
			}
		}()
		driver.outcome.restart, driver.outcome.err = body(ctx)
	}()
	return driver
}

// acpTestsStartDrive spawns drive over transport.
func acpTestsStartDrive(transport ByteStreams, spec LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent, opened *Shared[*OpenedSession], replacingPreviousBridge bool) *acpTestsDriver {
	return acpTestsSpawn(func(ctx context.Context) (SessionRestart, error) {
		return drive(ctx, transport, spec, requests, events, opened, replacingPreviousBridge)
	})
}

// acpTestsStartRun spawns the whole runtime, as mj_core::runtime::spawn(run(..))
// does. Aborting it is its shutdown signal.
func acpTestsStartRun(spec LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent) *acpTestsDriver {
	return acpTestsSpawn(func(ctx context.Context) (SessionRestart, error) {
		return nil, RunWithShutdown(ctx, spec, requests, events)
	})
}

// abort is JoinHandle::abort: the runtime stops at its next await point.
func (d *acpTestsDriver) abort() { d.cancel() }

func (d *acpTestsDriver) isFinished() bool {
	select {
	case <-d.finished:
		return true
	default:
		return false
	}
}

// wait is `timeout(timeout, driver)`: the runtime's outcome, failing the test
// with what when it does not finish in time, and when it panicked.
func (d *acpTestsDriver) wait(t *testing.T, timeout time.Duration, what string) (SessionRestart, error) {
	t.Helper()
	select {
	case <-d.finished:
	case <-time.After(timeout):
		t.Fatal(what)
	}
	if d.outcome.panicked != nil {
		t.Fatalf("the runtime task panicked: %v", d.outcome.panicked)
	}
	return d.outcome.restart, d.outcome.err
}

// waitBriefly is `let _ = timeout(timeout, driver).await`: wait for the
// runtime, but neither its result nor a timeout fails the test.
func (d *acpTestsDriver) waitBriefly(timeout time.Duration) {
	select {
	case <-d.finished:
	case <-time.After(timeout):
	}
}

func (d *acpTestsDriver) describe() string {
	if !d.isFinished() {
		return "still running"
	}
	if d.outcome.panicked != nil {
		return fmt.Sprintf("panicked: %v", d.outcome.panicked)
	}
	return fmt.Sprintf("restart %#v, error %v", d.outcome.restart, d.outcome.err)
}

// acpTestsNext is `timeout(timeout, event_rx.recv())` on a channel only the
// runtime's sender keeps open: the next event; closed once the runtime has
// ended and every event it sent was read; timedOut when neither happens.
func acpTestsNext(events <-chan core.RuntimeEvent, driver *acpTestsDriver, timeout time.Duration) (event core.RuntimeEvent, closed, timedOut bool) {
	timer := time.NewTimer(timeout)
	defer timer.Stop()
	var finished <-chan struct{}
	if driver != nil {
		finished = driver.finished
	}
	select {
	case event := <-events:
		return event, false, false
	case <-finished:
		select {
		case event := <-events:
			return event, false, false
		default:
			return nil, true, false
		}
	case <-timer.C:
		return nil, false, true
	}
}

// acpTestsExpectEvent is the next event, failing the test with timeoutText
// when none arrives in time and with closedText once the runtime has ended.
func acpTestsExpectEvent(t *testing.T, events <-chan core.RuntimeEvent, driver *acpTestsDriver, timeout time.Duration, timeoutText, closedText string) core.RuntimeEvent {
	t.Helper()
	event, closed, timedOut := acpTestsNext(events, driver, timeout)
	if timedOut {
		t.Fatal(timeoutText)
	}
	if closed {
		t.Fatalf("%s: %s", closedText, driver.describe())
	}
	return event
}

// acpTestsTryRecv is Receiver::try_recv.
func acpTestsTryRecv[T any](channel <-chan T) (T, bool) {
	select {
	case value := <-channel:
		return value, true
	default:
		var zero T
		return zero, false
	}
}

// acpTestsReceive is `timeout(timeout, rx).await`: the value, failing the
// test with what when it does not arrive in time.
func acpTestsReceive[T any](t *testing.T, channel <-chan T, timeout time.Duration, what string) T {
	t.Helper()
	select {
	case value := <-channel:
		return value
	case <-time.After(timeout):
		t.Fatal(what)
	}
	panic("unreachable")
}

// acpTestsDrain reads events until the runtime has ended, so a bounded
// channel never holds its shutdown (Rust's drain task).
func acpTestsDrain(events <-chan core.RuntimeEvent, driver *acpTestsDriver) <-chan struct{} {
	done := make(chan struct{})
	go func() {
		defer close(done)
		for {
			select {
			case <-events:
			case <-driver.finished:
				for {
					select {
					case <-events:
					default:
						return
					}
				}
			}
		}
	}()
	return done
}

// acpTestsListen is a loopback TCP listener on a free port (Rust's
// tokio::net::TcpListener::bind("127.0.0.1:0")).
func acpTestsListen() (*net.TCPListener, error) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, err
	}
	return listener.(*net.TCPListener), nil
}

// acpTestsAcceptWithin is `timeout(bound, listener.accept())`: the first
// connection, or an error once bound passes without one.
func acpTestsAcceptWithin(listener *net.TCPListener, bound time.Duration) (net.Conn, error) {
	if err := listener.SetDeadline(time.Now().Add(bound)); err != nil {
		return nil, err
	}
	defer listener.SetDeadline(time.Time{})
	return listener.Accept()
}

// acpTestsGo runs a fake bridge on its own goroutine and reports when it has
// returned (Rust's spawned bridge task).
func acpTestsGo(bridge func()) <-chan struct{} {
	done := make(chan struct{})
	go func() {
		defer close(done)
		bridge()
	}()
	return done
}

// titlingCodexBridge is a fake Codex bridge that titles a thread the way
// codex-acp 1.13.2 does: from the text blocks of the prompt, joined with
// spaces (`params.prompt.filter((b) => b.type === "text")` in its prompt
// handler, which feeds both the fallback title and the title model). Any
// other block is left out. It reports each prompt it receives, then the
// title, then one agent message so the test knows the title has been
// handled.
func titlingCodexBridge(t *testing.T, stream io.ReadWriteCloser, prompts chan<- any) {
	bridge := acpTestsBridge(stream)
	for {
		request, ok := bridge.next(t)
		if !ok {
			return
		}
		method, ok := request["method"].(string)
		if !ok {
			continue
		}
		id := request["id"]
		var messages []any
		switch {
		case method == "initialize":
			messages = []any{acpTestsResult(id, map[string]any{"protocolVersion": 1})}
		case method == "session/new":
			messages = []any{acpTestsResult(id, map[string]any{
				"sessionId": "titled",
				"modes": map[string]any{
					"currentModeId":  "agent",
					"availableModes": []any{map[string]any{"id": "agent", "name": "Agent"}},
				},
			})}
		case method == "session/prompt":
			prompt := acpTestsGet(request, "params", "prompt")
			var texts []string
			for _, block := range acpTestsArray(prompt) {
				if acpTestsGet(block, "type") != "text" {
					continue
				}
				if text, ok := acpTestsString(acpTestsGet(block, "text")); ok {
					texts = append(texts, text)
				}
			}
			title := strings.Join(texts, " ")
			prompts <- prompt
			messages = []any{
				acpTestsUpdate("titled", map[string]any{
					"sessionUpdate": "session_info_update",
					"title":         strings.TrimSpace(title),
				}),
				acpTestsUpdate("titled", map[string]any{
					"sessionUpdate": "agent_message_chunk",
					"content":       map[string]any{"type": "text", "text": "pong"},
				}),
				acpTestsResult(id, map[string]any{"stopReason": "end_turn"}),
			}
		case id != nil:
			messages = []any{acpTestsResult(id, map[string]any{})}
		default:
			continue
		}
		for _, message := range messages {
			if !bridge.send(message) {
				return
			}
		}
	}
}

// codexSessionTitles drives a new Codex session through one prompt against
// titlingCodexBridge. Returns the prompt the bridge received and the titles
// the worker relayed. projectMemory nil means no project memory.
func codexSessionTitles(t *testing.T, projectMemory *string, prompt []schema.ContentBlock) (any, []string) {
	t.Helper()
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	prompts := make(chan any, 16)
	go titlingCodexBridge(t, bridgeStream, prompts)
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	spec := reloadFallbackSpec(core.HarnessKindCodex)
	spec.ResumeSession = nil
	if projectMemory != nil {
		spec.ProjectMemory = &core.ProjectMemoryLaunchConfig{
			ProjectKey:   "abc",
			Root:         filepath.Join(*projectMemory, "memory"),
			BaselineRoot: filepath.Join(*projectMemory, "baseline"),
			MCPDelivery:  core.ProjectMemoryMcpDeliveryAcp,
		}
	}
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
	requests <- CommandRequestPrompt{RequestID: "first", Prompt: prompt}

	var titles []string
	for {
		event := acpTestsExpectEvent(t, events, driver, 5*time.Second, "the runtime keeps reporting", "the runtime stopped")
		update, ok := acpTestsUpdateValue(t, event)
		if !ok {
			continue
		}
		if acpTestsGet(update, "sessionUpdate") == "agent_message_chunk" {
			break
		}
		if acpTestsGet(update, "sessionUpdate") == "session_info_update" {
			if title, ok := acpTestsString(acpTestsGet(update, "title")); ok {
				titles = append(titles, title)
			}
		}
	}
	received := acpTestsReceive(t, prompts, ioTimeout, "the bridge received the prompt")
	driver.abort()
	bridgeStream.Close()
	return received, titles
}

func workerSocket(role core.SubagentMcpRole) SubagentMcpSocket {
	return SubagentMcpSocket{
		Path:                  "/worker/subagents.sock",
		Role:                  role,
		AgentMailboxesEnabled: true,
		SubtleSubagents:       true,
		ReviewTools:           false,
		ProfileRegistration:   false,
	}
}

// scriptedBridge answers `initialize` and `session/new`, then fails the
// first `session/prompt` with a JSON-RPC error and completes the second.
// Returns how many prompts it received.
func scriptedBridge(t *testing.T, stream io.ReadWriteCloser, loginPath string, loginIdentity *ClaudeLoginIdentity) int {
	bridge := acpTestsBridge(stream)
	prompts := 0
	for {
		request, ok := bridge.next(t)
		if !ok {
			return prompts
		}
		method, ok := request["method"].(string)
		if !ok {
			continue
		}
		id := request["id"]
		var response any
		switch method {
		case "initialize":
			response = acpTestsResult(id, map[string]any{"protocolVersion": 1})
		case "session/new":
			response = acpTestsResult(id, map[string]any{"sessionId": "scripted", "modes": map[string]any{
				"currentModeId": "default",
				"availableModes": []any{
					map[string]any{"id": "default", "name": "Default"},
					map[string]any{"id": "auto", "name": "Auto"},
				},
			}})
		case "session/set_mode":
			response = acpTestsResult(id, map[string]any{})
		case "session/prompt":
			prompts++
			if prompts == 1 {
				// Delivery precedes failure: the diagnostic must retain the
				// login from before this prompt, not reread this one.
				if err := os.WriteFile(loginPath, []byte(`{"claudeAiOauth":{"accessToken":"new-access","expiresAt":2000}}`), 0o600); err != nil {
					t.Errorf("write the rotated login: %v", err)
					return prompts
				}
				fingerprint := core.CredentialFingerprint([]byte("new-access"))
				loginIdentity.installed(&fingerprint)
				response = map[string]any{
					"jsonrpc": "2.0",
					"id":      id,
					"error":   map[string]any{"code": -32000, "message": "Authentication required"},
				}
			} else {
				if prompts == 3 {
					if !bridge.send(acpTestsUpdate("scripted", map[string]any{
						"sessionUpdate": "agent_message_chunk",
						"content":       map[string]any{"type": "text", "text": "answer"},
					})) {
						return prompts
					}
				}
				response = acpTestsResult(id, map[string]any{
					"stopReason": "end_turn",
					"usage":      map[string]any{"totalTokens": 30, "inputTokens": 20, "outputTokens": 10},
				})
			}
		default:
			continue
		}
		if !bridge.send(response) {
			return prompts
		}
	}
}

// extRequestBridge answers `initialize` and `session/new`, then — while the
// prompt is in flight — sends the client an ext request and publishes the
// answer as soon as it arrives, so a silent client shows up as a timeout.
func extRequestBridge(t *testing.T, stream io.ReadWriteCloser, method string, answered chan<- map[string]any) {
	bridge := acpTestsBridge(stream)
	published := false
	for {
		message, ok := bridge.next(t)
		if !ok {
			return
		}
		if message["id"] == "ext-1" {
			if !published {
				published = true
				answered <- message
			}
			continue
		}
		requestMethod, ok := message["method"].(string)
		if !ok {
			continue
		}
		id := message["id"]
		var response any
		switch requestMethod {
		case "initialize":
			response = acpTestsResult(id, map[string]any{"protocolVersion": 1})
		case "session/new":
			response = acpTestsResult(id, map[string]any{"sessionId": "scripted"})
		case "session/prompt":
			// Ask the client to leave plan mode without answering the
			// prompt: the turn only ends once the client replies, which is
			// exactly the hang this guards against.
			response = map[string]any{
				"jsonrpc": "2.0",
				"id":      "ext-1",
				"method":  method,
				"params": map[string]any{
					"sessionId":   "scripted",
					"toolCallId":  "call-1",
					"planContent": "1. do the thing",
				},
			}
		default:
			continue
		}
		if !bridge.send(response) {
			return
		}
	}
}

func answerToExtRequest(t *testing.T, method string, executionPolicy core.ExecutionPolicy) map[string]any {
	t.Helper()
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	answered := make(chan map[string]any, 1)
	go extRequestBridge(t, bridgeStream, method, answered)

	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	spec := acpTestsSpec(core.HarnessKindGrok, "scripted", acpTestsCurrentDir(t))
	spec.ExecutionPolicy = executionPolicy
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
	// Drain events so a full channel can never be mistaken for silence.
	acpTestsDrain(events, driver)
	requests <- acpTestsPromptRequest("first", "plan it")

	answer := acpTestsReceive(t, answered, 5*time.Second,
		"Hel must answer every incoming request instead of leaving the agent waiting")

	close(requests)
	driver.waitBriefly(5 * time.Second)
	bridgeStream.Close()
	driver.abort()
	return answer
}

func architectureForm() map[string]any {
	return map[string]any{
		"jsonrpc": "2.0",
		"id":      "ask-1",
		"method":  "elicitation/create",
		"params": map[string]any{
			"sessionId":  "scripted",
			"toolCallId": "question-tool",
			"mode":       "form",
			"message":    "Choose an architecture",
			"requestedSchema": map[string]any{
				"type":     "object",
				"required": []any{"architecture"},
				"properties": map[string]any{
					"architecture": map[string]any{
						"type":  "string",
						"title": "Architecture",
						"oneOf": []any{
							map[string]any{"const": "thin", "title": "Thin callers"},
							map[string]any{"const": "dynamic", "title": "Dynamic matrix"},
						},
					},
					"token": map[string]any{
						"type":  "string",
						"title": "Access token",
						"_meta": map[string]any{"codex": map[string]any{"isSecret": true}},
					},
				},
			},
		},
	}
}

// elicitationBridge scripts one prompt that asks the architecture question.
func elicitationBridge(t *testing.T, stream io.ReadWriteCloser, initialized, answered chan<- map[string]any, finishBeforeAnswer bool) {
	bridge := acpTestsBridge(stream)
	initializedSent, answeredSent := false, false
	var promptID any
	havePrompt := false
	for {
		message, ok := bridge.next(t)
		if !ok {
			return
		}
		if message["id"] == "ask-1" {
			if !answeredSent {
				answeredSent = true
				answered <- message
			}
			if havePrompt {
				havePrompt = false
				if !bridge.send(acpTestsResult(promptID, map[string]any{"stopReason": "end_turn"})) {
					t.Errorf("finish prompt")
					return
				}
			}
			continue
		}
		method, ok := message["method"].(string)
		if !ok {
			continue
		}
		id := message["id"]
		var response any
		switch method {
		case "initialize":
			if !initializedSent {
				initializedSent = true
				initialized <- message
			}
			response = acpTestsResult(id, map[string]any{"protocolVersion": 1})
		case "session/new":
			response = acpTestsResult(id, map[string]any{"sessionId": "scripted", "modes": map[string]any{
				"currentModeId": "default",
				"availableModes": []any{
					map[string]any{"id": "default", "name": "Default"},
					map[string]any{"id": "auto", "name": "Auto"},
					map[string]any{"id": "agent", "name": "Guardian"},
				},
			}})
		case "session/set_mode":
			response = acpTestsResult(id, map[string]any{})
		case "session/prompt":
			promptID, havePrompt = id, true
			response = architectureForm()
		default:
			continue
		}
		if !bridge.send(response) {
			return
		}
		if method == "session/prompt" && finishBeforeAnswer {
			havePrompt = false
			if !bridge.send(acpTestsResult(promptID, map[string]any{"stopReason": "end_turn"})) {
				return
			}
		}
	}
}

// answerArchitectureForm drives one prompt whose question the person
// answers. It fails if any form other than the architecture question reaches
// the person first.
func answerArchitectureForm(t *testing.T, harness core.HarnessKind, finishBeforeAnswer bool) {
	t.Helper()
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	initializedChannel := make(chan map[string]any, 1)
	answeredChannel := make(chan map[string]any, 1)
	bridgeDone := acpTestsGo(func() {
		elicitationBridge(t, bridgeStream, initializedChannel, answeredChannel, finishBeforeAnswer)
	})
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	spec := acpTestsSpec(harness, "scripted", acpTestsCurrentDir(t))
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
	initialized := acpTestsReceive(t, initializedChannel, 5*time.Second, "runtime initializes")
	if _, ok := acpTestsGet(initialized, "params", "clientCapabilities", "elicitation", "form").(map[string]any); !ok {
		t.Fatalf("the client must advertise form elicitation: %v", initialized)
	}

	requests <- acpTestsPromptRequest("prompt-1", "plan it")
	promptFinished := false
	var request core.ElicitationRequest
	for {
		event := acpTestsExpectEvent(t, events, driver, 5*time.Second, "elicitation arrives", "runtime event channel stays open")
		if finished, ok := event.(core.RuntimeEventPromptFinished); ok {
			if finished.StopReason != "EndTurn" {
				t.Fatalf("stop reason %q, want EndTurn", finished.StopReason)
			}
			promptFinished = true
		}
		if requested, ok := event.(core.RuntimeEventElicitationRequested); ok {
			request = requested.Request
			break
		}
	}
	if request.Message != "Choose an architecture" {
		t.Fatalf("message %q", request.Message)
	}
	if request.Fields[0].Title != "Architecture" {
		t.Fatalf("first field %q", request.Fields[0].Title)
	}
	if finishBeforeAnswer && !promptFinished {
		for {
			event := acpTestsExpectEvent(t, events, driver, 5*time.Second,
				"async questions must not block the prompt finishing", "the runtime stopped")
			if finished, ok := event.(core.RuntimeEventPromptFinished); ok {
				if finished.StopReason != "EndTurn" {
					t.Fatalf("stop reason %q, want EndTurn", finished.StopReason)
				}
				break
			}
			if _, resolved := event.(core.RuntimeEventElicitationResolved); resolved {
				t.Fatalf("the question resolved before it was answered: %#v", event)
			}
		}
	}
	refused := make(chan error, 1)
	requests <- CommandRequestResolveElicitation{
		ElicitationID: request.ID,
		Response: core.ElicitationResponse{Action: core.ElicitationResponseAccept, Content: map[string]core.ElicitationValue{
			"architecture": core.ElicitationValueString("unoffered"),
		}},
		Resolved: refused,
	}
	if err := acpTestsReceive(t, refused, ioTimeout, "the runtime answers the resolution"); err == nil {
		t.Fatal("an answer the form does not offer must be refused")
	}

	resolvedChannel := make(chan error, 1)
	requests <- CommandRequestResolveElicitation{
		ElicitationID: request.ID,
		Response: core.ElicitationResponse{Action: core.ElicitationResponseAccept, Content: map[string]core.ElicitationValue{
			"architecture": core.ElicitationValueString("thin"),
			"token":        core.ElicitationValueString("private-token"),
		}},
		Resolved: resolvedChannel,
	}
	if err := acpTestsReceive(t, resolvedChannel, ioTimeout, "the runtime answers the resolution"); err != nil {
		t.Fatalf("the offered answer must be accepted: %v", err)
	}
	answered := acpTestsReceive(t, answeredChannel, 5*time.Second, "bridge receives answer")
	if acpTestsGet(answered, "result", "action") != "accept" ||
		acpTestsGet(answered, "result", "content", "architecture") != "thin" ||
		acpTestsGet(answered, "result", "content", "token") != "private-token" {
		t.Fatalf("the bridge received %v", answered)
	}

	var resolution core.RuntimeEventElicitationResolved
	for {
		event := acpTestsExpectEvent(t, events, driver, 5*time.Second,
			"the accepted reply reaches conversation history", "the runtime stopped")
		if resolved, ok := event.(core.RuntimeEventElicitationResolved); ok {
			resolution = resolved
			break
		}
	}
	encoded, err := json.Marshal(resolution)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(encoded), "private-token") {
		t.Fatalf("the resolution leaked the secret: %s", encoded)
	}
	if resolution.ElicitationID != request.ID || resolution.Action != "accept" {
		t.Fatalf("resolution %#v", resolution)
	}
	wantReply := "Choose an architecture\n\nArchitecture: Thin callers\nAccess token: [hidden]"
	if resolution.Reply == nil || *resolution.Reply != wantReply {
		t.Fatalf("reply %v, want %q", resolution.Reply, wantReply)
	}

	relay, err := DurableRelayOpen(t.TempDir(), "elicitation-echo", "1.0.0")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := relay.RecordObservation(core.RelayObservationElicitationRequested{Request: request}); err != nil {
		t.Fatal(err)
	}
	if _, err := relay.RecordObservation(core.RelayObservationElicitationResolved{
		ElicitationID: resolution.ElicitationID,
		Action:        resolution.Action,
		Reply:         resolution.Reply,
	}); err != nil {
		t.Fatal(err)
	}
	session := core.MaterializedSessionEmpty("elicitation-echo")
	relayEvents, err := relay.EventsAfter(0, core.RelayEventGenesisDigest)
	if err != nil {
		t.Fatal(err)
	}
	for index := range relayEvents {
		projected, err := transcript.ProjectRelayEvent(&session, &relayEvents[index])
		if err != nil {
			t.Fatal(err)
		}
		if err := transcript.ApplyCommittedProjectionEvent(&session, &relayEvents[index], projected.Mutation); err != nil {
			t.Fatal(err)
		}
	}
	if len(session.PendingElicitations) != 0 {
		t.Fatalf("pending elicitations %v", session.PendingElicitations)
	}
	if len(session.Transcript) != 1 {
		t.Fatalf("transcript %v", session.Transcript)
	}
	if _, user := session.Transcript[0].Body.(core.TranscriptBodyUser); !user {
		t.Fatalf("the reply is a user item: %#v", session.Transcript[0].Body)
	}
	if session.Transcript[0].IsTurnStart() {
		t.Fatal("a form reply does not start a turn")
	}
	if text := transcript.TranscriptItemText(session.Transcript[0]); text != wantReply {
		t.Fatalf("transcript text %q", text)
	}

	duplicate := make(chan error, 1)
	requests <- CommandRequestResolveElicitation{
		ElicitationID: request.ID,
		Response:      core.ElicitationResponse{Action: core.ElicitationResponseCancel},
		Resolved:      duplicate,
	}
	if err := acpTestsReceive(t, duplicate, ioTimeout, "the runtime answers the resolution"); err == nil {
		t.Fatal("a second answer must be refused")
	}
	for {
		event, ok := acpTestsTryRecv(events)
		if !ok {
			break
		}
		if _, resolved := event.(core.RuntimeEventElicitationResolved); resolved {
			t.Fatal("a duplicate answer must not echo twice")
		}
	}

	close(requests)
	if _, err := driver.wait(t, 5*time.Second, "runtime exits"); err != nil {
		t.Fatalf("runtime exits cleanly: %v", err)
	}
	// The ended runtime has dropped its end of the stream.
	client.Close()
	acpTestsReceive(t, bridgeDone, ioTimeout, "the bridge finishes")
}

// modeSurface is how a fake bridge offers its modes.
type modeSurface int

const (
	modeSurfaceBoth modeSurface = iota
)

// modeChangeBridge offers its modes on surface. ignoredMode is the startup
// enforcement selection a test treats as setup rather than as the mode change
// it is observing; nil means none.
func modeChangeBridge(t *testing.T, stream io.ReadWriteCloser, surface modeSurface, ignoredMode *string, observed chan<- map[string]any) {
	modeOption := func(current string) map[string]any {
		return map[string]any{
			"id":           "interaction_mode",
			"name":         "Mode",
			"category":     "mode",
			"type":         "select",
			"currentValue": current,
			"options": []any{
				map[string]any{"value": "default", "name": "Default"},
				map[string]any{"value": "acceptEdits", "name": "Accept edits"},
				map[string]any{"value": "plan", "name": "Plan"},
				map[string]any{"value": "agent", "name": "Agent"},
				map[string]any{"value": "auto", "name": "Auto"},
				map[string]any{"value": "bypassPermissions", "name": "Bypass"},
				map[string]any{"value": "agent-full-access", "name": "Full access"},
			},
		}
	}
	modes := func() map[string]any {
		return map[string]any{
			// The target policy must be selected before exposing the session.
			"currentModeId": "agent",
			"availableModes": []any{
				map[string]any{"id": "default", "name": "Default"},
				map[string]any{"id": "acceptEdits", "name": "Accept edits"},
				map[string]any{"id": "plan", "name": "Plan"},
				map[string]any{"id": "agent", "name": "Agent"},
				map[string]any{"id": "auto", "name": "Auto"},
				map[string]any{"id": "bypassPermissions", "name": "Bypass"},
				map[string]any{"id": "agent-full-access", "name": "Full access"},
			},
		}
	}
	isIgnored := func(message map[string]any) bool {
		if ignoredMode == nil {
			return false
		}
		for _, key := range []string{"value", "modeId"} {
			if acpTestsGet(message, "params", key) == *ignoredMode {
				return true
			}
		}
		return false
	}
	bridge := acpTestsBridge(stream)
	published := false
	observe := func(message map[string]any) {
		if !isIgnored(message) && !published {
			published = true
			observed <- message
		}
	}
	for {
		message, ok := bridge.next(t)
		if !ok {
			return
		}
		method, ok := message["method"].(string)
		if !ok {
			continue
		}
		id := message["id"]
		var response any
		switch method {
		case "initialize":
			response = acpTestsResult(id, map[string]any{"protocolVersion": 1, "agentCapabilities": map[string]any{"loadSession": true}})
		case "session/new", "session/load":
			result := map[string]any{"sessionId": "scripted"}
			if surface == modeSurfaceBoth {
				result["configOptions"] = []any{modeOption("agent")}
				result["modes"] = modes()
			}
			response = acpTestsResult(id, result)
		case "session/set_config_option":
			observe(message)
			selected, ok := acpTestsString(acpTestsGet(message, "params", "value"))
			if !ok {
				selected = "default"
			}
			response = acpTestsResult(id, map[string]any{"configOptions": []any{modeOption(selected)}})
		default:
			observe(message)
			response = acpTestsResult(id, map[string]any{})
		}
		if !bridge.send(response) {
			return
		}
	}
}

func policyIsEnforcedBeforeSessionIsReported(t *testing.T, harness core.HarnessKind, executionPolicy core.ExecutionPolicy, claudePermissionMode core.ClaudePermissionMode, resumeSession *string) {
	t.Helper()
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	observed := make(chan map[string]any, 1)
	go modeChangeBridge(t, bridgeStream, modeSurfaceBoth, nil, observed)
	requests := make(chan CommandRequest, 1)
	events := make(chan core.RuntimeEvent, 16)
	spec := acpTestsSpec(harness, "scripted", acpTestsCurrentDir(t))
	spec.ResumeSession = resumeSession
	if resumeSession != nil {
		spec.NativeSessionHistory = NativeSessionHistoryMayHaveHistory
	} else {
		spec.NativeSessionHistory = NativeSessionHistoryLoadBeforeReplace
	}
	spec.ExecutionPolicy = executionPolicy
	spec.ApprovalChoices = core.DefaultApprovalChoices()
	spec.ApprovalChoices.ClaudePermissionMode = claudePermissionMode
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)

	var expectedMode, expectedLabel string
	switch {
	case harness == core.HarnessKindCodex && executionPolicy == core.ExecutionPolicyConfiguredApprovals:
		expectedMode, expectedLabel = "agent", "agent / guardian"
	case harness == core.HarnessKindCodex && executionPolicy == core.ExecutionPolicyUnconstrained:
		expectedMode, expectedLabel = "agent-full-access", "agent-full-access"
	case harness == core.HarnessKindClaude && executionPolicy == core.ExecutionPolicyConfiguredApprovals:
		switch claudePermissionMode {
		case core.ClaudePermissionModeAuto:
			expectedMode, expectedLabel = "auto", "auto / guardian"
		case core.ClaudePermissionModeAcceptEdits:
			expectedMode, expectedLabel = "acceptEdits", "acceptEdits / guardian"
		case core.ClaudePermissionModeManual:
			expectedMode, expectedLabel = "default", "default / guardian"
		}
	case harness == core.HarnessKindClaude && executionPolicy == core.ExecutionPolicyUnconstrained:
		expectedMode, expectedLabel = "bypassPermissions", "bypassPermissions / sandbox-off"
	default:
		t.Fatalf("unscripted harness %v under %v", harness, executionPolicy)
	}
	request := acpTestsReceive(t, observed, 5*time.Second, "Hel must enforce the target execution policy")
	if request["method"] != "session/set_config_option" {
		t.Fatalf("the policy is selected through its config option: %v", request)
	}
	if acpTestsGet(request, "params", "value") != expectedMode {
		t.Fatalf("selected %v, want %s", acpTestsGet(request, "params", "value"), expectedMode)
	}

	var reportedMode *string
	var reportedResumed *bool
	var configuredMode *string
	for reportedMode == nil || configuredMode == nil {
		event := acpTestsExpectEvent(t, events, driver, 5*time.Second,
			"the configured session must be reported", "the runtime must keep its event channel open")
		switch event := event.(type) {
		case core.RuntimeEventSessionStarted:
			reportedMode = event.ExecutionMode
			resumed := event.Resumed
			reportedResumed = &resumed
		case core.RuntimeEventSessionConfigured:
			options := acpTestsPlain(t, event.ConfigOptions)
			current, _ := acpTestsString(acpTestsGet(options, 0, "currentValue"))
			configuredMode = &current
		}
	}
	if *reportedMode != expectedLabel {
		t.Fatalf("reported mode %q, want %q", *reportedMode, expectedLabel)
	}
	if reportedResumed == nil || *reportedResumed != (resumeSession != nil) {
		t.Fatalf("reported resumed %v for resume %v", reportedResumed, resumeSession)
	}
	if *configuredMode != expectedMode {
		t.Fatalf("configured mode %q, want %q", *configuredMode, expectedMode)
	}

	close(requests)
	driver.waitBriefly(5 * time.Second)
	bridgeStream.Close()
	driver.abort()
}

// stubbornModeBridge is a harness that answers the mode request while still
// reporting another mode has not applied the policy, so the session must
// fail instead of running prompts under the permissions the operator did not
// ask for.
func stubbornModeBridge(t *testing.T, stream io.ReadWriteCloser) {
	modeOption := map[string]any{
		"id":       "interaction_mode",
		"name":     "Mode",
		"category": "mode",
		"type":     "select",
		// Never moves, whatever is selected.
		"currentValue": "default",
		"options": []any{
			map[string]any{"value": "default", "name": "Default"},
			map[string]any{"value": "bypassPermissions", "name": "Bypass"},
		},
	}
	bridge := acpTestsBridge(stream)
	for {
		message, ok := bridge.next(t)
		if !ok {
			return
		}
		method, ok := message["method"].(string)
		if !ok {
			continue
		}
		var result any
		switch method {
		case "initialize":
			result = map[string]any{"protocolVersion": 1}
		case "session/new":
			result = map[string]any{"sessionId": "scripted", "configOptions": []any{modeOption}}
		case "session/set_config_option":
			result = map[string]any{"configOptions": []any{modeOption}}
		default:
			result = map[string]any{}
		}
		if !bridge.send(acpTestsResult(message["id"], result)) {
			return
		}
	}
}

// allowAllOnlyMuseBridge is a Muse bridge whose approval selector offers only
// `allowAll`: a Guardian session has no approval mode to run in. With plan,
// it also carries Plan on its separate `mode` selector.
func allowAllOnlyMuseBridge(t *testing.T, stream io.ReadWriteCloser, plan bool) {
	options := []any{
		map[string]any{"id": "approval_mode", "name": "Approvals", "type": "select",
			"currentValue": "allowAll", "options": []any{map[string]any{"value": "allowAll", "name": "Allow all"}}},
		map[string]any{"id": "auto_review", "name": "Auto-review", "type": "select",
			"currentValue": "off", "options": []any{
				map[string]any{"value": "off", "name": "Off"},
				map[string]any{"value": "on", "name": "On"},
			}},
	}
	if plan {
		options = append(options, map[string]any{"id": "mode", "name": "Mode", "category": "mode", "type": "select",
			"currentValue": "default", "options": []any{
				map[string]any{"value": "default", "name": "Default"},
				map[string]any{"value": "plan", "name": "Plan"},
			}})
	}
	bridge := acpTestsBridge(stream)
	for {
		message, ok := bridge.next(t)
		if !ok {
			return
		}
		method, ok := message["method"].(string)
		if !ok {
			continue
		}
		var result any
		switch method {
		case "initialize":
			result = map[string]any{"protocolVersion": 1}
		case "session/new":
			result = map[string]any{"sessionId": "scripted", "configOptions": options}
		case "session/set_config_option":
			params, _ := message["params"].(map[string]any)
			selectOption(options, params)
			result = map[string]any{"configOptions": options}
		default:
			result = map[string]any{}
		}
		if !bridge.send(acpTestsResult(message["id"], result)) {
			return
		}
	}
}

func guardianMuseOfferingOnlyAllowAllFails(t *testing.T, plan bool) {
	t.Helper()
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	go allowAllOnlyMuseBridge(t, bridgeStream, plan)
	requests := make(chan CommandRequest, 1)
	events := make(chan core.RuntimeEvent, 16)
	spec := acpTestsSpec(core.HarnessKindMuse, "scripted", acpTestsCurrentDir(t))
	if plan {
		spec.ContextRestore = &ContextReset{RequestID: "restore", Mode: acpTestsStringPointer("plan")}
	}
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)

	_, err := driver.wait(t, 5*time.Second, "the session must stop instead of waiting for prompts")
	if err == nil {
		t.Fatal("a Guardian Muse session without an approval mode must fail")
	}
	if message := err.Error(); !strings.Contains(message, "does not offer the promptUnmatched mode") {
		t.Fatal(message)
	}
	close(requests)
	bridgeStream.Close()
}

// codexRefreshFailure is Codex's error when OpenAI rejects its refresh token
// (R14-1; reverify-14 cli/011, cli/013).
const codexRefreshFailure = "Your access token could not be refreshed. Please log out and sign in again."

// codexRefreshFailureBridge fails every prompt the way codex-acp 1.13.3
// failed one in reverify-14 (tmux/012): Codex's own account of the turn
// arrives as agent text, then the prompt fails with the same sentence and
// `codexErrorInfo: "unauthorized"`. The first prompt streams that text, the
// second streams nothing, and the third streams the sentence and then a
// thought, which ends that agent message.
func codexRefreshFailureBridge(t *testing.T, stream io.ReadWriteCloser) {
	bridge := acpTestsBridge(stream)
	prompts := 0
	for {
		request, ok := bridge.next(t)
		if !ok {
			return
		}
		method, ok := request["method"].(string)
		if !ok {
			continue
		}
		id := request["id"]
		var messages []any
		switch {
		case method == "initialize":
			messages = []any{acpTestsResult(id, map[string]any{"protocolVersion": 1})}
		case method == "session/new":
			messages = []any{acpTestsResult(id, map[string]any{
				"sessionId": "refresh-failure",
				"modes": map[string]any{
					"currentModeId":  "agent",
					"availableModes": []any{map[string]any{"id": "agent", "name": "Agent"}},
				},
			})}
		case method == "session/prompt":
			prompts++
			var streamed []any
			switch prompts {
			case 1:
				streamed = []any{
					map[string]any{
						"sessionUpdate": "agent_message_chunk",
						"content": map[string]any{"type": "text", "text": "Warning: Falling back from WebSockets to HTTPS transport. " +
							"stream disconnected before completion: websocket closed by server before response.completed\n\n"},
					},
					map[string]any{
						"sessionUpdate": "agent_message_chunk",
						"content":       map[string]any{"type": "text", "text": codexRefreshFailure},
					},
				}
			case 2:
			default:
				streamed = []any{
					map[string]any{
						"sessionUpdate": "agent_message_chunk",
						"content":       map[string]any{"type": "text", "text": codexRefreshFailure},
					},
					map[string]any{
						"sessionUpdate": "agent_thought_chunk",
						"content":       map[string]any{"type": "text", "text": "Retrying."},
					},
				}
			}
			for _, update := range streamed {
				messages = append(messages, acpTestsUpdate("refresh-failure", update))
			}
			messages = append(messages, map[string]any{
				"jsonrpc": "2.0",
				"id":      id,
				"error": map[string]any{
					"code":    -32603,
					"message": "Internal error",
					"data": map[string]any{
						"message":        codexRefreshFailure,
						"codexErrorInfo": "unauthorized",
					},
				},
			})
		case id != nil:
			messages = []any{acpTestsResult(id, map[string]any{})}
		default:
			continue
		}
		for _, message := range messages {
			if !bridge.send(message) {
				return
			}
		}
	}
}

// stalledPromptBridge answers `initialize` and `session/new`, then holds
// `session/prompt` until the test completes it. Used to prove cancel waits
// for a real prompt settlement and restarts when that settlement never
// arrives. Closing complete ends the bridge, as dropping Rust's sender does.
func stalledPromptBridge(t *testing.T, stream io.ReadWriteCloser, observed chan<- string, complete <-chan struct{}) {
	bridge := acpTestsBridge(stream)
	lines := acpTestsLines(t, bridge)
	var promptID any
	havePrompt := false
	for {
		select {
		case request, ok := <-lines:
			if !ok {
				return
			}
			method, ok := request["method"].(string)
			if !ok {
				continue
			}
			observed <- method
			id := request["id"]
			var response any
			switch method {
			case "initialize":
				response = acpTestsResult(id, map[string]any{"protocolVersion": 1})
			case "session/new", "session/load":
				response = acpTestsResult(id, map[string]any{"sessionId": "scripted"})
			case "session/prompt":
				promptID, havePrompt = id, true
				continue
			default:
				continue
			}
			if !bridge.send(response) {
				return
			}
		case _, ok := <-complete:
			if !ok {
				return
			}
			if !havePrompt {
				continue
			}
			havePrompt = false
			if !bridge.send(acpTestsResult(promptID, map[string]any{"stopReason": "cancelled"})) {
				return
			}
		}
	}
}

// musePolicyOptions is muse-acp 0.10's policy selectors as a fake bridge
// reports them: approvals on `approval_mode`, apart from Mode, and
// auto-review off in every session it opens. A guardian Muse session selects
// `promptUnmatched` and `on`.
func musePolicyOptions() []any {
	return []any{
		map[string]any{"id": "approval_mode", "name": "Approvals", "type": "select",
			"currentValue": "allowAll", "options": []any{
				map[string]any{"value": "allowAll", "name": "Allow all"},
				map[string]any{"value": "promptUnmatched", "name": "Prompt unmatched"},
			}},
		map[string]any{"id": "auto_review", "name": "Auto-review", "type": "select",
			"currentValue": "off", "options": []any{
				map[string]any{"value": "off", "name": "Off"},
				map[string]any{"value": "on", "name": "On"},
			}},
	}
}

// selectOption applies a `session/set_config_option` request's params to
// options, in place.
func selectOption(options []any, params map[string]any) {
	for _, option := range options {
		option, ok := option.(map[string]any)
		if !ok {
			panic("config options are a list of objects")
		}
		if option["id"] == params["configId"] {
			option["currentValue"] = params["value"]
		}
	}
}

// silentAfterPromptBridge is a bridge that accepts a prompt, optionally
// announces one tool call, and then goes silent for good. This is what a
// harness blocked in a long build looks like from Mjolnir: an open tool call
// and no protocol traffic at all.
func silentAfterPromptBridge(t *testing.T, stream io.ReadWriteCloser, observed chan<- string, openAToolCall bool) {
	silentAfterPromptBridgeWithLateReply(t, stream, observed, openAToolCall, false, false)
}

func silentAfterPromptBridgeWithLateReply(t *testing.T, stream io.ReadWriteCloser, observed chan<- string, openAToolCall, lateReply, childTraffic bool) {
	bridge := acpTestsBridge(stream)
	lines := acpTestsLines(t, bridge)
	var priorPrompt any
	havePrior := false
	traffic := time.NewTicker(100 * time.Millisecond)
	defer traffic.Stop()
	childStarted := false
	options := musePolicyOptions()
	for {
		var request map[string]any
		var tick <-chan time.Time
		if childStarted {
			tick = traffic.C
		}
		select {
		case line, ok := <-lines:
			if !ok {
				return
			}
			request = line
		case <-tick:
			for _, update := range []struct {
				session string
				update  map[string]any
			}{
				{"scripted", map[string]any{"sessionUpdate": "subagent_state_update", "subagentSessionId": "heap", "state": "running"}},
				{"heap", map[string]any{"sessionUpdate": "agent_message_chunk", "content": map[string]any{"type": "text", "text": "Heap analysis continues."}}},
				{"heap", map[string]any{"sessionUpdate": "tool_call", "toolCallId": "heap-tool", "title": "inspect heap", "status": "in_progress"}},
			} {
				if !bridge.send(acpTestsUpdate(update.session, update.update)) {
					return
				}
			}
			continue
		}
		method, ok := request["method"].(string)
		if !ok {
			continue
		}
		observed <- method
		id := request["id"]
		var response any
		switch {
		case method == "initialize":
			response = acpTestsResult(id, map[string]any{"protocolVersion": 1})
		case (method == "session/new" || method == "session/load") && childTraffic:
			response = acpTestsResult(id, map[string]any{"sessionId": "scripted", "modes": map[string]any{
				"currentModeId": "auto", "availableModes": []any{map[string]any{"id": "auto", "name": "Auto"}},
			}})
		case method == "session/set_mode":
			response = acpTestsResult(id, map[string]any{})
		case method == "session/new" || method == "session/load":
			response = acpTestsResult(id, map[string]any{"sessionId": "scripted", "configOptions": options})
		case method == "session/set_config_option":
			params, _ := request["params"].(map[string]any)
			selectOption(options, params)
			response = acpTestsResult(id, map[string]any{"configOptions": options})
		case method == "session/prompt":
			if childTraffic && !childStarted {
				if !bridge.send(acpTestsUpdate("scripted", map[string]any{
					"sessionUpdate": "subagent_spawned", "subagentSessionId": "heap", "name": "heap",
					"task": "Independent heap analysis", "capabilities": map[string]any{},
				})) {
					t.Errorf("announce the child")
					return
				}
				childStarted = true
			}
			if lateReply {
				prior, hadPrior := priorPrompt, havePrior
				priorPrompt, havePrior = id, true
				if hadPrior {
					if !bridge.send(acpTestsUpdate("scripted", map[string]any{
						"sessionUpdate": "agent_message_chunk",
						"content":       map[string]any{"type": "text", "text": "Late harness content remains visible."},
					})) {
						return
					}
					if !bridge.send(acpTestsResult(prior, map[string]any{"stopReason": "end_turn"})) {
						return
					}
				}
			}
			if openAToolCall {
				if !bridge.send(acpTestsUpdate("scripted", map[string]any{
					"sessionUpdate": "tool_call",
					"toolCallId":    "long-build",
					"title":         "cargo nextest run",
					"status":        "in_progress",
				})) {
					return
				}
			}
			// Never answer: the turn is now as silent as a harness blocked
			// inside one long tool call.
			continue
		default:
			continue
		}
		if !bridge.send(response) {
			return
		}
	}
}

// silentBridgeSpec is the launch of a scripted Muse bridge under
// stallPolicy: Muse marks no turn end of its own, so the watchdog covers it.
// Shared with the guardian tests.
func silentBridgeSpec(stallPolicy core.StallPolicy) LaunchSpec {
	directory, err := os.Getwd()
	if err != nil {
		panic(err)
	}
	spec := acpTestsSpec(core.HarnessKindMuse, "scripted", directory)
	spec.StallPolicy = &stallPolicy
	return spec
}

// waitForBridgePrompt reads scripted-bridge methods into methods until a
// `session/prompt` arrives, so a test can assert what the bridge saw and in
// what order.
func waitForBridgePrompt(t *testing.T, observed <-chan string, methods *[]string) {
	t.Helper()
	for {
		var method string
		select {
		case seen, ok := <-observed:
			if !ok {
				t.Fatal("the bridge must keep reporting methods")
			}
			method = seen
		case <-time.After(5 * time.Second):
			t.Fatal("the prompt must reach the bridge")
		}
		*methods = append(*methods, method)
		if method == "session/prompt" {
			return
		}
	}
}

// promptEchoingBridge answers `initialize`, `session/load`, and every
// `session/prompt` at once, reporting each prompt's text so a test can tell
// which prompts reached it.
func promptEchoingBridge(t *testing.T, stream io.ReadWriteCloser, observed chan<- string) {
	promptEchoingBridgeReporting(t, stream, observed, nil)
}

// promptEchoingBridgeReporting is promptEchoingBridge that names itself
// agentVersion in `initialize`; nil names no version.
func promptEchoingBridgeReporting(t *testing.T, stream io.ReadWriteCloser, observed chan<- string, agentVersion *string) {
	bridge := acpTestsBridge(stream)
	for {
		request, ok := bridge.next(t)
		if !ok {
			return
		}
		method, ok := request["method"].(string)
		if !ok {
			continue
		}
		id := request["id"]
		var response any
		switch method {
		case "initialize":
			observed <- method
			result := map[string]any{"protocolVersion": 1}
			if agentVersion != nil {
				result["agentInfo"] = map[string]any{"name": "opencode", "version": *agentVersion}
			}
			response = acpTestsResult(id, result)
		case "session/new", "session/load":
			observed <- method
			response = acpTestsResult(id, map[string]any{"sessionId": "scripted"})
		case "session/prompt":
			text, _ := acpTestsString(acpTestsGet(request, "params", "prompt", 0, "text"))
			observed <- "session/prompt:" + text
			response = acpTestsResult(id, map[string]any{"stopReason": "end_turn"})
		default:
			continue
		}
		if !bridge.send(response) {
			return
		}
	}
}

// waitForRuntimeEvent returns the first event from events that matches,
// failing the test after the shared I/O hang guard.
func waitForRuntimeEvent(t *testing.T, events <-chan core.RuntimeEvent, matches func(core.RuntimeEvent) bool) core.RuntimeEvent {
	t.Helper()
	timeout := time.After(ioTimeout)
	for {
		select {
		case event, ok := <-events:
			if !ok {
				t.Fatal("runtime event channel stays open")
			}
			if matches(event) {
				return event
			}
		case <-timeout:
			t.Fatal("timed out waiting for runtime event")
		}
	}
}

func steeringBridge(t *testing.T, stream io.ReadWriteCloser, observed chan<- any, complete <-chan struct{}, steeringOutcome string) {
	bridge := acpTestsBridge(stream)
	lines := acpTestsLines(t, bridge)
	var promptID, heldSteeringID any
	havePrompt, haveHeldSteering := false, false
	for {
		select {
		case request, ok := <-lines:
			if !ok {
				return
			}
			method, ok := request["method"].(string)
			if !ok {
				continue
			}
			observed <- request
			id := request["id"]
			var response any
			switch {
			case method == "initialize":
				response = acpTestsResult(id, map[string]any{
					"protocolVersion": 1,
					"_meta":           map[string]any{"steering": map[string]any{"supported": true}},
				})
			case method == "session/new":
				response = acpTestsResult(id, map[string]any{
					"sessionId": "steering-session",
					"modes": map[string]any{
						"currentModeId": "agent",
						"availableModes": []any{
							map[string]any{"id": "agent", "name": "Agent"},
							map[string]any{"id": "agent-full-access", "name": "Full access"},
						},
					},
				})
			case method == "session/set_mode":
				response = acpTestsResult(id, map[string]any{})
			case method == "session/prompt":
				promptID, havePrompt = id, true
				continue
			case method == sessionSteeringMethod && steeringOutcome == "hold":
				heldSteeringID, haveHeldSteering = id, true
				continue
			case method == sessionSteeringMethod:
				response = acpTestsResult(id, map[string]any{"outcome": steeringOutcome})
			default:
				continue
			}
			if !bridge.send(response) {
				return
			}
		case _, ok := <-complete:
			if !ok {
				return
			}
			if haveHeldSteering {
				haveHeldSteering = false
				if !bridge.send(acpTestsResult(heldSteeringID, map[string]any{"outcome": "injected"})) {
					t.Errorf("answer the held steer")
					return
				}
				continue
			}
			if !havePrompt {
				continue
			}
			havePrompt = false
			if !bridge.send(acpTestsResult(promptID, map[string]any{"stopReason": "end_turn"})) {
				return
			}
		}
	}
}

func exerciseImageSteering(t *testing.T, withImages bool, steeringOutcome string) {
	t.Helper()
	imagesRoot := t.TempDir()
	store := core.AttachmentStoreWorker(imagesRoot)
	var imageBlocks []schema.ContentBlock
	var expectedImages []string
	if withImages {
		for index := range 10 {
			bytes, err := base64.StdEncoding.DecodeString("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
			if err != nil {
				t.Fatal(err)
			}
			for len(bytes) < core.MaxImageBytes {
				bytes = append(bytes, byte(index))
			}
			reference, err := core.NewAttachmentRef(bytes, "image/png", 1, 1)
			if err != nil {
				t.Fatal(err)
			}
			if err := store.Install(&reference, bytes); err != nil {
				t.Fatal(err)
			}
			block := acpTestsDecode[schema.ContentBlock](t, string(acpTestsMust(t)(json.Marshal(reference.ContentBlock()))))
			imageBlocks = append(imageBlocks, block)
			expectedImages = append(expectedImages, base64.StdEncoding.EncodeToString(bytes))
		}
	}
	blocks := func(text string) []schema.ContentBlock {
		return append([]schema.ContentBlock{acpTestsText(text)}, imageBlocks...)
	}
	assertImages := func(request any) {
		t.Helper()
		prompt := acpTestsArray(acpTestsGet(request, "params", "prompt"))
		if len(prompt) != len(expectedImages)+1 {
			t.Fatalf("the prompt carries %d blocks, want %d", len(prompt), len(expectedImages)+1)
		}
		for index, expected := range expectedImages {
			image := prompt[index+1]
			if acpTestsGet(image, "data") != expected {
				t.Fatalf("image %d does not carry its bytes", index)
			}
			if acpTestsGet(image, "uri") != nil {
				t.Fatalf("image %d keeps its store uri", index)
			}
		}
	}
	client, bridgeStream := acpTestsDuplex(64 * 1024)
	observed := make(chan any, 256)
	complete := make(chan struct{}, 1)
	go steeringBridge(t, bridgeStream, observed, complete, steeringOutcome)
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	spec := acpTestsSpec(core.HarnessKindCodex, "scripted", acpTestsCurrentDir(t))
	driver := acpTestsStartDrive(acpTestsTransport(client), spec, requests, events, NewShared[*OpenedSession](nil), false)
	defer func() {
		bridgeStream.Close()
		driver.abort()
	}()

	connected := acpTestsExpectEvent(t, events, driver, ioTimeout, "the runtime connects", "the runtime stopped")
	if event, ok := connected.(core.RuntimeEventConnected); !ok || event.SteeringSupported == nil || !*event.SteeringSupported {
		t.Fatalf("the first event reports steering support: %#v", connected)
	}

	if withImages {
		requests <- CommandRequestPromptAttachments{RequestID: "prompt-1", Prompt: blocks("start"), Root: imagesRoot}
	} else {
		requests <- CommandRequestPrompt{RequestID: "prompt-1", Prompt: blocks("start")}
	}
	for {
		request := acpTestsReceive(t, observed, ioTimeout, "prompt reaches bridge")
		if acpTestsGet(request, "method") == "session/prompt" {
			assertImages(request)
			break
		}
	}
	var attachmentRoot *string
	if withImages {
		attachmentRoot = &imagesRoot
	}
	requests <- CommandRequestSteer{
		RequestID:      "cancel-1",
		ActivePromptID: "prompt-1",
		SteeringPrompt: core.ClaimedSteeringPrompt{
			AttachmentRoot:  attachmentRoot,
			QueuedCommandID: "queued-1",
			Prompt:          blocks("change direction"),
		},
	}

	var steering any
	for {
		request := acpTestsReceive(t, observed, ioTimeout, "steer reaches bridge")
		if acpTestsGet(request, "method") == sessionSteeringMethod {
			steering = request
			break
		}
		if acpTestsGet(request, "method") == "session/cancel" {
			t.Fatal("steering must not cancel")
		}
	}
	assertImages(steering)
	if acpTestsGet(steering, "params", "sessionId") != "steering-session" {
		t.Fatalf("steer session %v", acpTestsGet(steering, "params", "sessionId"))
	}
	if acpTestsGet(steering, "params", "prompt", 0, "text") != "change direction" {
		t.Fatalf("steer text %v", acpTestsGet(steering, "params", "prompt", 0, "text"))
	}
	if acpTestsGet(steering, "params", "_meta", "steering", "idleBehavior") != "promptRequired" {
		t.Fatalf("steer idle behavior %v", acpTestsGet(steering, "params", "_meta"))
	}
	if steeringOutcome == "hold" {
		// Rust advances a paused clock past the 30-second acknowledgement
		// bound; Go waits for it in real time.
		acpTestsWaitForEventWithin(t, events, 30*time.Second+ioTimeout, func(event core.RuntimeEvent) bool {
			_, ok := event.(core.RuntimeEventSteeringUnconfirmed)
			return ok
		})
		requests <- CommandRequestSteer{
			RequestID:      "steer-repeat",
			ActivePromptID: "prompt-1",
			SteeringPrompt: core.ClaimedSteeringPrompt{
				QueuedCommandID: "queued-1",
				Prompt:          blocks("change direction"),
			},
		}
		waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
			rejected, ok := event.(core.RuntimeEventCommandRejected)
			return ok && rejected.RequestID == "steer-repeat"
		})
		if request, ok := acpTestsTryRecv(observed); ok {
			t.Fatalf("a repeated steer must not cancel or inject again: %v", request)
		}
		complete <- struct{}{}
	}
	switch steeringOutcome {
	case "failed":
		waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
			rejected, ok := event.(core.RuntimeEventCommandRejected)
			return ok && rejected.RequestID == "cancel-1"
		})
	case "startedNewTurn":
		waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
			interrupted, ok := event.(core.RuntimeEventCommandInterrupted)
			return ok && interrupted.RequestID == "cancel-1"
		})
	case "promptRequired":
		waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
			returned, ok := event.(core.RuntimeEventSteerReturned)
			return ok && returned.RequestID == "cancel-1" && returned.QueuedCommandID == "queued-1"
		})
	default:
		waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
			applied, ok := event.(core.RuntimeEventSteerApplied)
			return ok && applied.RequestID == "cancel-1" && applied.QueuedCommandID == "queued-1"
		})
	}
	if request, ok := acpTestsTryRecv(observed); ok {
		t.Fatalf("steering must not send cancel: %v", request)
	}

	complete <- struct{}{}
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		finished, ok := event.(core.RuntimeEventPromptFinished)
		return ok && finished.RequestID == "prompt-1"
	})
	close(requests)
	if _, err := driver.wait(t, 5*time.Second, "runtime exits"); err != nil {
		t.Fatalf("steering does not fail the runtime: %v", err)
	}
}

// acpTestsWaitForEventWithin is waitForRuntimeEvent with its own bound, for
// a wait Rust covers by advancing a paused clock.
func acpTestsWaitForEventWithin(t *testing.T, events <-chan core.RuntimeEvent, bound time.Duration, matches func(core.RuntimeEvent) bool) core.RuntimeEvent {
	t.Helper()
	timeout := time.After(bound)
	for {
		select {
		case event := <-events:
			if matches(event) {
				return event
			}
		case <-timeout:
			t.Fatal("timed out waiting for runtime event")
		}
	}
}

// acpTestsMust unwraps a (value, error) pair, failing the test on the error.
func acpTestsMust(t *testing.T) func([]byte, error) []byte {
	return func(value []byte, err error) []byte {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
		return value
	}
}

// sessionReloadRejectingBridge is a fake bridge that rejects every attempt to
// reload a recorded session and answers `session/new` with a fresh id.
func sessionReloadRejectingBridge(t *testing.T, stream io.ReadWriteCloser, observed chan<- string, advertisedResume bool) {
	bridge := acpTestsBridge(stream)
	for {
		request, ok := bridge.next(t)
		if !ok {
			return
		}
		method, ok := request["method"].(string)
		if !ok {
			continue
		}
		observed <- method
		id := request["id"]
		var response any
		switch {
		case method == "initialize":
			result := map[string]any{"protocolVersion": 1}
			if advertisedResume {
				result["agentCapabilities"] = map[string]any{
					"sessionCapabilities": map[string]any{"resume": map[string]any{}},
				}
			}
			response = acpTestsResult(id, result)
		case method == "session/load" || method == "session/resume":
			response = map[string]any{
				"jsonrpc": "2.0",
				"id":      id,
				"error": map[string]any{
					"code":    -32602,
					"message": "Session not found: it no longer exists in the backend",
				},
			}
		case method == "session/new":
			response = acpTestsResult(id, map[string]any{
				"sessionId": "fresh",
				"modes": map[string]any{
					"currentModeId":  "build",
					"availableModes": []any{map[string]any{"id": "build", "name": "Build"}},
				},
			})
		// Anything else session setup sends (mode or config selection)
		// succeeds trivially.
		case id != nil:
			response = acpTestsResult(id, map[string]any{})
		default:
			continue
		}
		if !bridge.send(response) {
			return
		}
	}
}

// reloadFallbackSpec is the launch of a scripted bridge resuming the
// recorded session "gone". Shared with the instructions tests.
func reloadFallbackSpec(harness core.HarnessKind) LaunchSpec {
	directory, err := os.Getwd()
	if err != nil {
		panic(err)
	}
	spec := acpTestsSpec(harness, "scripted", directory)
	spec.ResumeSession = acpTestsStringPointer("gone")
	if harness == core.HarnessKindCodex {
		spec.NativeSessionHistory = NativeSessionHistoryMayHaveHistory
	} else {
		spec.NativeSessionHistory = NativeSessionHistoryLoadBeforeReplace
	}
	return spec
}

func exerciseContextClear(t *testing.T, failReplacement bool, harness core.HarnessKind) {
	t.Helper()
	temp := t.TempDir()
	script := filepath.Join(temp, "clear.py")
	if err := os.WriteFile(script, []byte(`
import json, os, sys
root = sys.argv[1]
fail = sys.argv[2] == 'true'
count_path = os.path.join(root, 'generation')
generation = int(open(count_path).read()) + 1 if os.path.exists(count_path) else 1
with open(count_path, 'w') as f: f.write(str(generation))
options = [{'id':'fast','name':'Fast','type':'select','currentValue':'true' if generation == 1 else 'false','options':[{'value':'true','name':'On'},{'value':'false','name':'Off'}]}]
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
        if generation == 2 and fail:
            print(json.dumps({'jsonrpc':'2.0','id':ident,'error':{'code':-32603,'message':'replacement refused'}}), flush=True)
            continue
        result = {'sessionId': 'original' if generation == 1 or method != 'session/new' else 'replacement',
                  'configOptions':options,
                  'modes': {'currentModeId':'plan' if generation == 1 else 'agent','availableModes':[{'id':'agent','name':'Agent'},{'id':'plan','name':'Plan'},{'id':'auto','name':'Auto'}]}}
    elif method == 'session/set_config_option':
        options[0]['currentValue'] = request['params']['value']
        result = {'configOptions':options}
    else:
        result = {}
    print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}), flush=True)
`), 0o644); err != nil {
		t.Fatal(err)
	}
	spec := silentBridgeSpec(core.StallPolicy{})
	spec.Command = "python3"
	spec.Args = []string{script, temp, fmt.Sprint(failReplacement)}
	spec.Cwd = temp
	spec.Harness = harness
	requests := make(chan CommandRequest, 4)
	events := make(chan core.RuntimeEvent, 64)
	runtime := acpTestsStartRun(spec, requests, events)
	defer runtime.abort()
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		_, ok := event.(core.RuntimeEventSessionConfigured)
		return ok
	})
	requests <- CommandRequestSetSessionMode{RequestID: "select-plan", ModeID: "plan"}
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		applied, ok := event.(core.RuntimeEventSessionModeApplied)
		return ok && applied.RequestID == "select-plan"
	})
	requests <- CommandRequestClearContext{RequestID: "clear-request"}
	// Hard-won: ad4c24b: bridge retirement must be published before replacement startup.
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		_, ok := event.(core.RuntimeEventBridgeRetired)
		return ok
	})
	// Hard-won: 2a0aae9: rollback skipped the old bridge's retirement signal.
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		clearing, ok := event.(core.RuntimeEventContextClearing)
		return ok && clearing.RequestID == "clear-request"
	})
	if failReplacement {
		waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
			rejected, ok := event.(core.RuntimeEventCommandRejected)
			return ok && rejected.RequestID == "clear-request" && strings.Contains(rejected.Message, "replacement refused")
		})
		if harness == core.HarnessKindCodex {
			// Codex keeps a never-prompted thread only in the app-server that
			// created it, and the rollback runs in a new bridge, so the
			// unused original is replaced rather than reloaded.
			waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
				started, ok := event.(core.RuntimeEventSessionStarted)
				return ok && started.NativeSessionID == "replacement" && !started.Resumed &&
					started.ReplacedUnusedNativeSessionID != nil && *started.ReplacedUnusedNativeSessionID == "original"
			})
		} else {
			waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
				started, ok := event.(core.RuntimeEventSessionStarted)
				return ok && started.NativeSessionID == "original" && started.Resumed
			})
		}
	} else {
		waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
			cleared, ok := event.(core.RuntimeEventContextCleared)
			return ok && cleared.RequestID == "clear-request" && cleared.NativeSessionID == "replacement"
		})
	}
	waitForRuntimeEvent(t, events, func(event core.RuntimeEvent) bool {
		_, ok := event.(core.RuntimeEventSessionConfigured)
		return ok
	})
	close(requests)
	// Drain the bounded event channel while shutdown completes.
	drained := acpTestsDrain(events, runtime)
	if _, err := runtime.wait(t, ioTimeout, "the runtime ends with its requests"); err != nil {
		t.Fatal(err)
	}
	<-drained
	recorded, err := os.ReadFile(filepath.Join(temp, "requests.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	var requestsSent []map[string]any
	var methods []string
	for _, line := range strings.Split(strings.TrimRight(string(recorded), "\n"), "\n") {
		request, err := acpTestsDecodeObject([]byte(line))
		if err != nil {
			t.Fatal(err)
		}
		requestsSent = append(requestsSent, request)
		method, _ := request["method"].(string)
		methods = append(methods, method)
	}
	if slices.Contains(methods, "session/prompt") {
		t.Fatalf("a clear must not forward a prompt: %v", methods)
	}
	if !slices.ContainsFunc(requestsSent, func(request map[string]any) bool {
		return request["method"] == "session/set_config_option" &&
			acpTestsGet(request, "params", "configId") == "fast" &&
			acpTestsGet(request, "params", "value") == "true"
	}) {
		t.Fatalf("the replacement must restore the fast selector: %v", methods)
	}
	planModes := 0
	for _, request := range requestsSent {
		if request["method"] == "session/set_mode" && acpTestsGet(request, "params", "modeId") == "plan" {
			planModes++
		}
	}
	if planModes != 2 {
		t.Fatalf("plan mode was selected %d times, want 2", planModes)
	}
	count := func(method string) int {
		n := 0
		for _, seen := range methods {
			if seen == method {
				n++
			}
		}
		return n
	}
	wantNew := 2
	if failReplacement && harness == core.HarnessKindCodex {
		wantNew = 3
	}
	if count("session/new") != wantNew {
		t.Fatalf("session/new sent %d times, want %d: %v", count("session/new"), wantNew, methods)
	}
	wantLoad := 0
	if failReplacement && harness != core.HarnessKindCodex {
		wantLoad = 1
	}
	if count("session/load") != wantLoad {
		t.Fatalf("session/load sent %d times, want %d: %v", count("session/load"), wantLoad, methods)
	}
}

func pendingTestQuestion(id string) core.ElicitationRequest {
	return core.ElicitationRequest{
		ID:             id,
		Message:        "",
		Title:          nil,
		Description:    nil,
		Fields:         []core.ElicitationField{},
		ToolPermission: nil,
	}
}

// typedFailureBridge is an adapter that, once asked for typed failures, ends
// the first prompt the way it ended issue 1217's: `end_turn`, no agent text,
// and the provider's error as a `sessionFailure` record on the response.
// Reports whether the worker's `initialize` asked for the records.
func typedFailureBridge(t *testing.T, stream io.ReadWriteCloser, failure any) bool {
	bridge := acpTestsBridge(stream)
	asked := false
	for {
		request, ok := bridge.next(t)
		if !ok {
			return asked
		}
		method, ok := request["method"].(string)
		if !ok {
			continue
		}
		id := request["id"]
		var result any
		switch {
		case method == "initialize":
			names, _ := acpTestsPointer(request, "/params/clientCapabilities/_meta/jetbrains/air/capabilities")
			asked = slices.Contains(acpTestsArray(names), any("sessionFailure"))
			result = map[string]any{"protocolVersion": 1}
		case method == "session/new":
			result = map[string]any{
				"sessionId": "typed-failure",
				"modes": map[string]any{
					"currentModeId": "agent",
					"availableModes": []any{
						map[string]any{"id": "agent", "name": "Agent"},
						map[string]any{"id": "auto", "name": "Auto"},
					},
				},
			}
		case method == "session/prompt":
			result = map[string]any{
				"stopReason": "end_turn",
				"_meta":      map[string]any{"jetbrains": map[string]any{"air": map[string]any{"version": 1, "sessionFailure": failure}}},
			}
		case id != nil:
			result = map[string]any{}
		default:
			continue
		}
		if !bridge.send(acpTestsResult(id, result)) {
			return asked
		}
	}
}

// heldPromptBridge is a fake OpenCode that opens one session and holds each
// prompt until release fires.
func heldPromptBridge(t *testing.T, stream io.ReadWriteCloser, observed chan<- string, release <-chan struct{}) {
	bridge := acpTestsBridge(stream)
	for {
		request, ok := bridge.next(t)
		if !ok {
			return
		}
		method, ok := request["method"].(string)
		if !ok {
			continue
		}
		observed <- method
		id := request["id"]
		var result any
		switch {
		case method == "initialize":
			result = map[string]any{"protocolVersion": 1}
		case method == "session/new":
			result = map[string]any{"sessionId": "ses_opencode"}
		case method == "session/prompt":
			<-release
			result = map[string]any{"stopReason": "end_turn"}
		case id != nil:
			result = map[string]any{}
		default:
			continue
		}
		if !bridge.send(acpTestsResult(id, result)) {
			return
		}
	}
}

// acpTestsWaitFor waits until the fake bridge has seen method. Rust's
// wait_for; prefixed because test_support has its own wait_for.
func acpTestsWaitFor(t *testing.T, observed <-chan string, method string) {
	t.Helper()
	timeout := time.After(5 * time.Second)
	for {
		select {
		case seen, ok := <-observed:
			if !ok {
				t.Fatalf("the fake bridge stopped before %s", method)
			}
			if seen == method {
				return
			}
		case <-timeout:
			t.Fatalf("no %s", method)
		}
	}
}

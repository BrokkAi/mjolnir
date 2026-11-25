//go:build unix

// Port of mj-worker/src/worker_runtime/relay_tests.rs.
//
// The relay tests of the worker runtime: they drive the relay coordinator,
// the control socket and the worker daemon over real sockets, files and
// processes. Rust compiles the module under cfg(all(test, unix)).
//
// Helpers whose Rust names other mj-worker modules also declare carry the
// prefix `runtimeRelayTests` (SESSION_ID, prompt, submit, agent_output and
// run_relay_coordinator); relay/tests.rs uses `relayTests`. Helpers that
// exist only in this Go port carry the prefix `runtimeRelay`.

package worker

import (
	"bufio"
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"maps"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"testing/synctest"
	"time"

	acp "github.com/BrokkAi/acp-go"
	acpschema "github.com/BrokkAi/acp-go/schema"
	"github.com/BurntSushi/toml"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
	"github.com/BrokkAi/mjolnir/go/internal/workerreview"
)

// runtimeRelayTestsSessionID is the session every fixture in this module
// opens (Rust's SESSION_ID).
const runtimeRelayTestsSessionID = "018f9dd2-a3b4-7c8d-9000-123456789abc"

// runtimeRelayOpen opens a durable relay in root for the fixture session
// (Rust's DurableRelay::open(..).unwrap()).
func runtimeRelayOpen(t *testing.T, root, relayVersion string) *DurableRelay {
	t.Helper()
	relay, err := DurableRelayOpen(root, runtimeRelayTestsSessionID, relayVersion)
	if err != nil {
		t.Fatalf("open relay in %s: %v", root, err)
	}
	return relay
}

// runtimeRelayCell opens a durable relay in root behind a RelayCell.
func runtimeRelayCell(t *testing.T, root, relayVersion string) *RelayCell {
	t.Helper()
	return NewRelayCell(runtimeRelayOpen(t, root, relayVersion))
}

// runtimeRelayPair is tokio's UnixStream::pair: two connected Unix stream
// sockets, closed when the test ends.
func runtimeRelayPair(t *testing.T) (*net.UnixConn, *net.UnixConn) {
	t.Helper()
	fds, err := syscall.Socketpair(syscall.AF_UNIX, syscall.SOCK_STREAM, 0)
	if err != nil {
		t.Fatal(err)
	}
	conns := make([]*net.UnixConn, 2)
	for index, fd := range fds {
		syscall.CloseOnExec(fd)
		file := os.NewFile(uintptr(fd), fmt.Sprintf("socketpair-%d", index))
		conn, err := net.FileConn(file)
		file.Close()
		if err != nil {
			t.Fatal(err)
		}
		conns[index] = conn.(*net.UnixConn)
		t.Cleanup(func() { conn.Close() })
	}
	return conns[0], conns[1]
}

// runtimeRelayDroppedWake is a dispatch wake channel whose receiver is
// already gone (Rust: `let (wake, _) = mpsc::channel(1)`).
func runtimeRelayDroppedWake() *dispatchWakeChannel {
	wake := newDispatchWakeChannel()
	wake.closeReceiver()
	return wake
}

// runtimeRelayFrame encodes envelope as one relay JSON line.
func runtimeRelayFrame(t *testing.T, envelope core.RelayRequestEnvelope) []byte {
	t.Helper()
	encoded, err := json.Marshal(envelope)
	if err != nil {
		t.Fatal(err)
	}
	return append(encoded, '\n')
}

// runtimeRelayWrite writes bytes to the connection or fails the test.
func runtimeRelayWrite(t *testing.T, writer io.Writer, frame []byte) {
	t.Helper()
	if _, err := writer.Write(frame); err != nil {
		t.Fatalf("write relay frame: %v", err)
	}
}

// runtimeRelaySend writes envelope as one frame.
func runtimeRelaySend(t *testing.T, writer io.Writer, envelope core.RelayRequestEnvelope) {
	t.Helper()
	runtimeRelayWrite(t, writer, runtimeRelayFrame(t, envelope))
}

type runtimeRelayLine struct {
	line string
	err  error
}

// runtimeRelayNextLine reads the next line within ioTimeout (Rust:
// timeout(IO_TIMEOUT, lines.next_line())). ok is false at the end of the
// stream.
func runtimeRelayNextLine(t *testing.T, lines *bufio.Reader) (string, bool) {
	t.Helper()
	done := make(chan runtimeRelayLine, 1)
	go func() {
		line, err := lines.ReadString('\n')
		done <- runtimeRelayLine{line: line, err: err}
	}()
	timer := time.NewTimer(ioTimeout)
	defer timer.Stop()
	select {
	case read := <-done:
		if read.err != nil {
			if errors.Is(read.err, io.EOF) && read.line == "" {
				return "", false
			}
			if !errors.Is(read.err, io.EOF) {
				t.Fatalf("read relay line: %v", read.err)
			}
		}
		line := strings.TrimSuffix(read.line, "\n")
		return strings.TrimSuffix(line, "\r"), true
	case <-timer.C:
		t.Fatal("timed out reading a relay line")
		return "", false
	}
}

// runtimeRelayReadLine reads the next line and requires one.
func runtimeRelayReadLine(t *testing.T, lines *bufio.Reader) string {
	t.Helper()
	line, ok := runtimeRelayNextLine(t, lines)
	if !ok {
		t.Fatal("the relay connection closed before answering")
	}
	return line
}

// runtimeRelayDecode decodes one relay response line.
func runtimeRelayDecode(t *testing.T, line string) core.RelayResponseEnvelope {
	t.Helper()
	var response core.RelayResponseEnvelope
	if err := json.Unmarshal([]byte(line), &response); err != nil {
		t.Fatalf("decode relay response %q: %v", line, err)
	}
	return response
}

// runtimeRelayReadResponse reads and decodes the next response.
func runtimeRelayReadResponse(t *testing.T, lines *bufio.Reader) core.RelayResponseEnvelope {
	t.Helper()
	return runtimeRelayDecode(t, runtimeRelayReadLine(t, lines))
}

// runtimeRelayExchange sends envelope and reads its response.
func runtimeRelayExchange(t *testing.T, lines *bufio.Reader, writer io.Writer, envelope core.RelayRequestEnvelope) core.RelayResponseEnvelope {
	t.Helper()
	runtimeRelaySend(t, writer, envelope)
	return runtimeRelayReadResponse(t, lines)
}

// runtimeRelayOk is the payload of a successful response; the test fails
// with context otherwise.
func runtimeRelayOk(t *testing.T, response core.RelayResponseEnvelope, context string) core.RelayResponsePayload {
	t.Helper()
	body, ok := response.Body.(core.RelayResponseBodyOk)
	if !ok {
		t.Fatalf("%s: %#v", context, response.Body)
	}
	return body.Payload
}

// runtimeRelayError is the protocol error of a failed response.
func runtimeRelayError(t *testing.T, response core.RelayResponseEnvelope, context string) core.RelayProtocolError {
	t.Helper()
	body, ok := response.Body.(core.RelayResponseBodyError)
	if !ok {
		t.Fatalf("%s: %#v", context, response)
	}
	return body.Error
}

// runtimeRelayJoin waits for a spawned task within ioTimeout and returns
// its result, failing when it panicked.
func runtimeRelayJoin[T any](t *testing.T, task *core.JoinHandle[T], description string) T {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), ioTimeout)
	defer cancel()
	value, err := task.Join(ctx)
	if err != nil {
		t.Fatalf("%s: %v", description, err)
	}
	return value
}

// runtimeRelayJoinOk waits for a task that returns an error and requires
// success (Rust: task.await.unwrap().unwrap()).
func runtimeRelayJoinOk(t *testing.T, task *core.JoinHandle[error], description string) {
	t.Helper()
	if err := runtimeRelayJoin(t, task, description); err != nil {
		t.Fatalf("%s: %v", description, err)
	}
}

func runtimeRelayTextBlock(text string) acpschema.ContentBlock {
	return acpschema.ContentBlock{Kind: acpschema.ContentBlockKindText, Text: &acpschema.TextContent{Text: text}}
}

func runtimeRelayFileMode(t *testing.T, path string) os.FileMode {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	return info.Mode().Perm()
}

func runtimeRelayReadFile(t *testing.T, path string) []byte {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return data
}

func runtimeRelayWriteFile(t *testing.T, path string, data []byte) {
	t.Helper()
	if err := os.WriteFile(path, data, 0o644); err != nil {
		t.Fatal(err)
	}
}

func runtimeRelayMkdirAll(t *testing.T, path string) {
	t.Helper()
	if err := os.MkdirAll(path, 0o755); err != nil {
		t.Fatal(err)
	}
}

func runtimeRelayExists(path string) bool {
	_, err := os.Lstat(path)
	return err == nil
}

func runtimeRelayMust(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

// delegationExchange sends one relay request over a served connection and
// returns the payload of its successful response.
func delegationExchange(t *testing.T, lines *bufio.Reader, writer io.Writer, request core.RelayRequest) core.RelayResponsePayload {
	t.Helper()
	runtimeRelaySend(t, writer, core.RelayRequestEnvelope{
		RequestID:       "delegation-regression",
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         request,
	})
	line := runtimeRelayReadLine(t, lines)
	if len(line) >= core.MaxFrameBytes {
		t.Fatalf("response line of %d bytes exceeds the frame limit", len(line))
	}
	response := runtimeRelayDecode(t, line)
	body, ok := response.Body.(core.RelayResponseBodyOk)
	if !ok {
		t.Fatalf("delegation exchange failed: %#v", response.Body)
	}
	return body.Payload
}

func projectMemoryTransferExchange(t *testing.T, lines *bufio.Reader, writer io.Writer, requestID string, action core.ProjectMemoryTransferAction) core.RelayResponseEnvelope {
	t.Helper()
	return projectMemoryTransferExchangeAt(t, lines, writer, requestID, core.RelayProtocolVersion, action)
}

func projectMemoryTransferExchangeAt(t *testing.T, lines *bufio.Reader, writer io.Writer, requestID string, protocolVersion uint32, action core.ProjectMemoryTransferAction) core.RelayResponseEnvelope {
	t.Helper()
	frame := runtimeRelayFrame(t, core.RelayRequestEnvelope{
		RequestID:       requestID,
		ProtocolVersion: protocolVersion,
		Request:         core.RelayRequestProjectMemoryTransfer{Action: action},
	})
	if len(frame) > core.MaxFrameBytes {
		t.Fatalf("request frame of %d bytes exceeds the frame limit", len(frame))
	}
	runtimeRelayWrite(t, writer, frame)
	line := runtimeRelayReadLine(t, lines)
	if len(line) >= core.MaxFrameBytes {
		t.Fatalf("response line of %d bytes exceeds the frame limit", len(line))
	}
	return runtimeRelayDecode(t, line)
}

func nextProjectMemoryTransferExchange(t *testing.T, lines *bufio.Reader, writer io.Writer, sequence *uint64, action core.ProjectMemoryTransferAction) core.RelayResponseEnvelope {
	t.Helper()
	requestID := fmt.Sprintf("project-memory-transfer-%d", *sequence)
	*sequence++
	return projectMemoryTransferExchange(t, lines, writer, requestID, action)
}

func uploadProjectMemoryBytes(t *testing.T, lines *bufio.Reader, writer io.Writer, sequence *uint64, data []byte, kind core.ProjectMemoryUploadKind) core.RelayResponsePayload {
	t.Helper()
	response := nextProjectMemoryTransferExchange(t, lines, writer, sequence, core.ProjectMemoryTransferActionBeginUpload{
		Kind:       kind,
		TotalBytes: uint64(len(data)),
	})
	started, ok := runtimeRelayTransferResult(response).(core.ProjectMemoryTransferResultUploadStarted)
	if !ok {
		t.Fatalf("project-memory upload did not start: %#v", response.Body)
	}
	transferID := started.TransferID
	capacity, err := core.ProjectMemoryTransferRequestChunkBytes(core.RelayProtocolVersion, "relay-ffffffffffffffff-18446744073709551615")
	if err != nil {
		t.Fatal(err)
	}
	offset := uint64(0)
	for chunk := range slices.Chunk(data, capacity) {
		response := nextProjectMemoryTransferExchange(t, lines, writer, sequence, core.ProjectMemoryTransferActionAppendUpload{
			TransferID: transferID,
			Offset:     offset,
			Chunk:      base64.StdEncoding.EncodeToString(chunk),
		})
		progress, ok := runtimeRelayTransferResult(response).(core.ProjectMemoryTransferResultUploadProgress)
		if !ok {
			t.Fatalf("project-memory upload page failed: %#v", response.Body)
		}
		if progress.TransferID != transferID {
			t.Fatalf("upload progress names transfer %d, want %d", progress.TransferID, transferID)
		}
		offset = progress.NextOffset
	}
	response = nextProjectMemoryTransferExchange(t, lines, writer, sequence, core.ProjectMemoryTransferActionCommitUpload{TransferID: transferID})
	body, ok := response.Body.(core.RelayResponseBodyOk)
	if !ok {
		t.Fatalf("project-memory upload commit failed: %#v", response.Body)
	}
	return body.Payload
}

// runtimeRelayTransferResult is the project-memory transfer result of a
// successful response, or nil.
func runtimeRelayTransferResult(response core.RelayResponseEnvelope) core.ProjectMemoryTransferResult {
	body, ok := response.Body.(core.RelayResponseBodyOk)
	if !ok {
		return nil
	}
	transfer, ok := body.Payload.(core.RelayResponsePayloadProjectMemoryTransfer)
	if !ok {
		return nil
	}
	return transfer.Result
}

// incompressibleMemoryText is length printable ASCII bytes from a xorshift
// sequence, which compression cannot shrink.
func incompressibleMemoryText(length int) string {
	state := uint64(0x9e37_79b9_7f4a_7c15)
	var text strings.Builder
	text.Grow(length)
	for range length {
		state ^= state << 13
		state ^= state >> 7
		state ^= state << 17
		text.WriteByte('!' + byte(state%90))
	}
	return text.String()
}

// projectMemorySocket serves one client connection with endpoint as its
// project-memory service, returning the client end and the serving task.
func projectMemorySocket(t *testing.T, relay *RelayCell, endpoint projectMemoryEndpoint) (*net.UnixConn, *core.JoinHandle[error]) {
	t.Helper()
	server, client := runtimeRelayPair(t)
	wake := runtimeRelayDroppedWake()
	runtime := defaultConnectionRuntime()
	runtime.projectMemory = endpoint
	fatal, _ := fatalReports()
	task := core.Spawn(func() error {
		return serveClientWithMemory(context.Background(), server, relay, wake, testCredentials(), runtime, fatal)
	})
	return client, task
}

// runtimeRelayEnqueue is the cfg(test) SubagentEndpoint::enqueue: queue
// request and return any result already recorded for it.
func runtimeRelayEnqueue(endpoint *SubagentEndpoint, request core.SubagentToolRequest) (*core.SubagentToolResult, error) {
	_, result, _, err := endpoint.enqueueMarkedAtOrigin(request, nil)
	return result, err
}

// runtimeRelaySubagentResults is the results half of the cfg(test)
// SubagentEndpoint::snapshot: every recorded completion's result.
func runtimeRelaySubagentResults(t *testing.T, endpoint *SubagentEndpoint) []core.SubagentToolResult {
	t.Helper()
	endpoint.state.mu.Lock()
	defer endpoint.state.mu.Unlock()
	state := &endpoint.state.state
	ids := slices.Sorted(maps.Keys(state.completions))
	results := []core.SubagentToolResult{}
	for _, id := range ids {
		result, err := endpoint.readResult(state, id)
		if err != nil {
			t.Fatal(err)
		}
		if result != nil {
			results = append(results, *result)
		}
	}
	return results
}

// Hard-won: 82e4c8a44: retained child reports made a healthy worker unreachable.
func TestOversizedLegacyDelegationHistoryReplaysOnlyCompletionReceipts(t *testing.T) {
	root := t.TempDir()
	results := map[string]core.SubagentToolResult{}
	for i := range 256 {
		id := fmt.Sprintf("historical-%03d", i)
		results[id] = core.SubagentToolResult{
			RequestID:     id,
			CompletedAtMs: int64(i),
			IsError:       false,
			Message:       "historical report " + strings.Repeat("λ", 18_000),
		}
	}
	body, err := json.Marshal(map[string]any{"requests": map[string]any{}, "results": results})
	runtimeRelayMust(t, err)
	if len(body) <= core.MaxFrameBytes {
		t.Fatalf("the legacy history must exceed one frame, got %d bytes", len(body))
	}
	runtimeRelayWriteFile(t, filepath.Join(root, "subagents.json"), body)
	endpoint, err := SubagentEndpointOpen(root)
	runtimeRelayMust(t, err)
	info, err := os.Stat(filepath.Join(root, "subagents.json"))
	runtimeRelayMust(t, err)
	if info.Size() >= 100_000 {
		t.Fatalf("the queue file kept %d bytes of reply bodies", info.Size())
	}
	relay := runtimeRelayCell(t, root, "test")
	wake := runtimeRelayDroppedWake()
	server, client := runtimeRelayPair(t)
	runtime := defaultConnectionRuntime()
	runtime.subagents = endpoint
	fatal, _ := fatalReports()
	task := core.Spawn(func() error {
		return serveClientWithMemory(context.Background(), server, relay, wake, testCredentials(), runtime, fatal)
	})
	lines := bufio.NewReader(client)
	first := delegationExchange(t, lines, client, core.RelayRequestPollSubagentQueue{Acknowledged: nil})
	replay := delegationExchange(t, lines, client, core.RelayRequestPollSubagentQueue{Acknowledged: nil})
	if !reflect.DeepEqual(first, replay) {
		t.Fatalf("a lost page acknowledgment must replay exactly: %#v != %#v", first, replay)
	}
	queue, ok := first.(core.RelayResponsePayloadSubagentQueue)
	if !ok {
		t.Fatal("missing queue")
	}
	page := queue.Page
	if len(page.Changes) != 256 {
		t.Fatalf("changes = %d, want 256", len(page.Changes))
	}
	for _, change := range page.Changes {
		if _, ok := change.(core.SubagentQueueChangeCompleted); !ok {
			t.Fatalf("expected only completion receipts, got %#v", change)
		}
	}
	sequence := page.Sequence
	next := delegationExchange(t, lines, client, core.RelayRequestPollSubagentQueue{Acknowledged: &sequence})
	queue, ok = next.(core.RelayResponsePayloadSubagentQueue)
	if !ok {
		t.Fatal("missing queue")
	}
	page = queue.Page
	if len(page.Changes) != 0 {
		t.Fatalf("unchanged polls must not retransmit history: %#v", page.Changes)
	}
	request := core.SubagentToolRequest{
		OriginatingCommandID: nil,
		RequestID:            "pending-after-reconnect",
		CreatedAtMs:          1000,
		Action:               core.SubagentToolActionListAgents{},
	}
	_, err = runtimeRelayEnqueue(endpoint, request)
	runtimeRelayMust(t, err)
	sequence = page.Sequence
	next = delegationExchange(t, lines, client, core.RelayRequestPollSubagentQueue{Acknowledged: &sequence})
	queue, ok = next.(core.RelayResponsePayloadSubagentQueue)
	if !ok {
		t.Fatal("missing queue")
	}
	if want := []core.SubagentQueueChange{core.SubagentQueueChangeRequest{Request: request}}; !reflect.DeepEqual(queue.Page.Changes, want) {
		t.Fatalf("changes = %#v, want %#v", queue.Page.Changes, want)
	}
	if _, ok := delegationExchange(t, lines, client, core.RelayRequestStatus{}).(core.RelayResponsePayloadStatus); !ok {
		t.Fatal("expected a status payload")
	}
	client.Close()
	runtimeRelayJoinOk(t, task, "serve the delegation connection")
	reopened, err := SubagentEndpointOpen(root)
	runtimeRelayMust(t, err)
	if got := len(runtimeRelaySubagentResults(t, reopened)); got != 256 {
		t.Fatalf("private reply bodies survive reopen: got %d results", got)
	}
}

// TestRuntimeEventSender sends runtime events without waiting for room, as
// Rust's try_send does.
type TestRuntimeEventSender struct {
	sender chan<- core.RuntimeEvent
}

func (s TestRuntimeEventSender) send(event core.RuntimeEvent) error {
	select {
	case s.sender <- event:
		return nil
	default:
		return errors.New("the runtime event channel is full")
	}
}

// close is Rust's drop of the last sender: the coordinator sees the event
// stream end.
func (s TestRuntimeEventSender) close() { close(s.sender) }

func runtimeEventChannel() (TestRuntimeEventSender, <-chan core.RuntimeEvent) {
	events := make(chan core.RuntimeEvent, acpEventChannelCapacity)
	return TestRuntimeEventSender{sender: events}, events
}

// runtimeRelayTestsRunRelayCoordinator runs the relay coordinator for a
// protocol fixture (Rust's run_relay_coordinator). Protocol fixtures have no
// classifier. Verdict fixtures supply their own local HTTP endpoint instead
// of resolving the host's default service. The test sends wakes through
// wakes and reads commands from commands.receiver().
func runtimeRelayTestsRunRelayCoordinator(ctx context.Context, relay *RelayCell, events <-chan core.RuntimeEvent, wakes *dispatchWakeChannel, commands *commandChannel) error {
	cwd, err := os.Getwd()
	if err != nil {
		return err
	}
	shellEvents := make(chan core.RuntimeEvent, 1)
	shells := NewUserShellRegistry(cwd, map[string]string{}, shellEvents)
	return runRelayCoordinatorWithVerdict(ctx, relay, events, wakes, commands, shells, nil, nil, nil)
}

// runtimeRelaySpawnCoordinator spawns the protocol fixture's coordinator;
// cancel stops it (Rust: abort).
func runtimeRelaySpawnCoordinator(relay *RelayCell, events <-chan core.RuntimeEvent, wakes *dispatchWakeChannel, commands *commandChannel) (*core.JoinHandle[error], context.CancelFunc) {
	ctx, cancel := context.WithCancel(context.Background())
	task := core.Spawn(func() error {
		return runtimeRelayTestsRunRelayCoordinator(ctx, relay, events, wakes, commands)
	})
	return task, cancel
}

// runtimeRelayAbort cancels a task and waits for it to stop (Rust:
// abort, then await).
func runtimeRelayAbort[T any](t *testing.T, task *core.JoinHandle[T], cancel context.CancelFunc) {
	t.Helper()
	cancel()
	ctx, stop := context.WithTimeout(context.Background(), ioTimeout)
	defer stop()
	select {
	case <-task.Done():
	case <-ctx.Done():
		t.Fatal("the aborted task did not stop")
	}
}

// fatalReports is a channel for fatal connection reports; Rust returns its
// sender and receiver.
func fatalReports() (chan<- error, <-chan error) {
	reports := make(chan error, 1)
	return reports, reports
}

// launchConfig is a Codex launch whose harness home is profileHome. The
// worker_runtime/unix tests share it.
func launchConfig(profileHome string) core.WorkerLaunchConfig {
	return core.WorkerLaunchConfig{
		CPUAccounting:         core.CpuAccountingModeProcessTree,
		NoGithubCredentials:   false,
		Oracle:                false,
		InitialCommands:       []core.InitialWorkerCommand{},
		ReviewerLaunch:        nil,
		Subagents:             core.SubagentPolicy{Mode: core.SubagentPolicyNative},
		HandbackTool:          false,
		SubagentKind:          nil,
		AgentMailboxesEnabled: true,
		SubtleSubagents:       true,
		InitialModel:          nil,
		ReviewTrigger:         core.ReviewTriggerOff,
		ReviewCapture:         false,
		GoalResumeRequest:     nil,
		TargetEnvironment:     map[string]string{},
		SeedImageEnvironment:  false,
		RunMode:               core.WorkerRunModeHarness,
		SessionID:             runtimeRelayTestsSessionID,
		Harness:               core.HarnessKindCodex,
		HarnessHome:           profileHome,
		AuthenticationMarker:  nil,
		BridgeCommand:         "/usr/bin/false",
		BridgeArgs:            []string{},
		HarnessRuntime:        core.HarnessRuntimePolicyAmbient,
		Environment:           map[string]string{"CODEX_HOME": profileHome},
		ExcludedEnvironment:   []string{},
		Cwd:                   ".local/share/hel/workspaces/session/repo",
		AdditionalDirectories: []string{},
		NativeSessionID:       nil,
		ProjectMemory:         nil,
		ExecutionPolicy:       core.ExecutionPolicyUnconstrained,
		ClaudePermissionMode:  core.ClaudePermissionModeAuto,
		MjolnirGuardian:       true,
		ObservePolicy:         nil,
	}
}

// expectDaemonLaunchFailure runs the worker daemon on config and waits until
// it records a launch failure or returns the error itself.
func expectDaemonLaunchFailure(t *testing.T, root string, config core.WorkerLaunchConfig) {
	t.Helper()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	daemon := core.Spawn(func() error { return RunDaemon(ctx, root, config) })
	deadline := time.Now().Add(ioTimeout)
	for {
		select {
		case <-daemon.Done():
			err, joinErr := daemon.Join(context.Background())
			if joinErr != nil {
				t.Fatalf("worker task panicked: %v", joinErr)
			}
			if err == nil {
				t.Fatal("the fixture cannot start an ACP supervisor")
			}
			return
		default:
		}
		failureRecorded := false
		if data, err := os.ReadFile(filepath.Join(root, core.WorkerStartupFile)); err == nil {
			var record map[string]any
			if json.Unmarshal(data, &record) == nil {
				_, failureRecorded = record["failure"].(map[string]any)
			}
		}
		if failureRecorded {
			runtimeRelayAbort(t, daemon, cancel)
			return
		}
		if !time.Now().Before(deadline) {
			cancel()
			<-daemon.Done()
			startup, err := os.ReadFile(filepath.Join(root, core.WorkerStartupFile))
			text := string(startup)
			if err != nil {
				text = fmt.Sprintf("cannot read startup record: %v", err)
			}
			t.Fatalf("fixture worker did not report its launch failure; startup record: %s", text)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

// stubRuntimeBridge writes an executable bridge that exits with status 1.
func stubRuntimeBridge(t *testing.T, root string) string {
	t.Helper()
	path := filepath.Join(root, "stub-bridge")
	if err := os.WriteFile(path, []byte("#!/bin/sh\nexit 1\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, 0o755); err != nil {
		t.Fatal(err)
	}
	return path
}

// gitRepository creates `repository` under temp with one commit on main.
func gitRepository(t *testing.T, temp string) string {
	t.Helper()
	repository := filepath.Join(temp, "repository")
	if err := os.Mkdir(repository, 0o755); err != nil {
		t.Fatal(err)
	}
	testSupportGit(repository, "init", "-q", "-b", "main")
	testSupportGit(repository, "config", "user.name", "Hel Test")
	testSupportGit(repository, "config", "user.email", "hel@example.test")
	runtimeRelayWriteFile(t, filepath.Join(repository, "tracked.rs"), []byte("fn main() {}\n"))
	testSupportGit(repository, "add", ".")
	testSupportGit(repository, "commit", "-qm", "base")
	return repository
}

// testCredentials is the credential endpoint of launchConfig("/profile"),
// or the reason it has none (Rust's Err(String), which the connection
// reports).
func testCredentials() credentialEndpointResult {
	config := launchConfig("/profile")
	endpoint, err := credentialEndpoint(&config)
	return credentialEndpointResult{endpoint: endpoint, err: err}
}

func codexCredentials(lastRefresh string) []byte {
	bytes, err := json.Marshal(map[string]any{
		"auth_mode":    "chatgpt",
		"tokens":       map[string]any{"access_token": "access", "refresh_token": "refresh"},
		"last_refresh": lastRefresh,
	})
	if err != nil {
		panic(err)
	}
	return bytes
}

func installRequest(bytes []byte) core.RelayRequest {
	return core.RelayRequestInstallCredentials{Data: base64.StdEncoding.EncodeToString(bytes)}
}

// credentialRequest answers request through the worker's credential handler
// for endpoint, returning the payload or the response's error message.
func credentialRequest(ctx context.Context, endpoint CredentialEndpoint, request core.RelayRequest) (core.RelayResponsePayload, error) {
	response := credentialResponse(ctx, core.RelayRequestEnvelope{
		RequestID:       "credential-request-test",
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         request,
	}, credentialEndpointResult{endpoint: endpoint}, endpoint.Home, &ClaudeLoginIdentity{})
	switch body := response.Body.(type) {
	case core.RelayResponseBodyOk:
		return body.Payload, nil
	case core.RelayResponseBodyError:
		return nil, errors.New(body.Error.Message)
	default:
		return nil, fmt.Errorf("unexpected credential response body %#v", response.Body)
	}
}

func skillsInstallRequest(bytes []byte) core.RelayRequest {
	return core.RelayRequestInstallSkills{Data: base64.StdEncoding.EncodeToString(bytes)}
}

func skillsStateOf(t *testing.T, payload core.RelayResponsePayload) core.SkillsSyncState {
	t.Helper()
	state, ok := payload.(core.RelayResponsePayloadSkillsState)
	if !ok {
		t.Fatalf("expected a skills state payload, got %#v", payload)
	}
	return core.SkillsSyncState{Present: state.Present, Fingerprint: state.Fingerprint}
}

// runtimeRelayRun runs command with input through the shared subprocess
// helper and returns its output.
func runtimeRelayRun(t *testing.T, command *exec.Cmd, input []byte) core.ProcessOutput {
	t.Helper()
	output, err := core.RunWithInput(command, input)
	if err != nil {
		t.Fatalf("run %s: %v", command.Path, err)
	}
	return output
}

// runtimeRelayEnvironList is an environment map as exec.Cmd.Env (Rust:
// env_clear().envs(map)).
func runtimeRelayEnvironList(environment map[string]string) []string {
	list := make([]string, 0, len(environment))
	for _, name := range slices.Sorted(maps.Keys(environment)) {
		list = append(list, name+"="+environment[name])
	}
	return list
}

func TestGithubCliWrapperReadsEachLiveTokenAndClearsStaleEnvironment(t *testing.T) {
	worker := t.TempDir()
	real := t.TempDir()
	realBin := filepath.Join(real, "bin")
	runtimeRelayMust(t, os.Mkdir(realBin, 0o755))
	core.InstallFakeCommand(realBin, "gh", "#!/bin/sh\nprintf '%s|%s\\n' \"${GH_TOKEN-unset}\" \"${GITHUB_TOKEN-unset}\"\n")

	environment := map[string]string{"PATH": realBin + ":/usr/bin:/bin"}
	runtimeRelayMust(t, ConfigureGithubCLI(worker, environment, nil, nil, false))
	tokenPath := filepath.Join(worker, "github-token")
	runtimeRelayMust(t, core.RemoveGithubToken(tokenPath))

	invoke := func(expectedToken *string) string {
		if expectedToken != nil {
			if _, err := core.WriteGithubToken(tokenPath, []byte(*expectedToken)); err != nil {
				t.Fatal(err)
			}
		} else {
			runtimeRelayMust(t, core.RemoveGithubToken(tokenPath))
		}
		command := exec.Command(filepath.Join(worker, "bin/gh"))
		command.Env = []string{"PATH=" + environment["PATH"], "GH_TOKEN=stale-token", "GITHUB_TOKEN=also-stale"}
		output := runtimeRelayRun(t, command, nil)
		if !output.Status.Success() {
			t.Fatalf("%s", fromUTF8Lossy(output.Stderr))
		}
		return string(output.Stdout)
	}

	first, rotated := "first-token", "rotated-token"
	if got := invoke(&first); got != "first-token|unset\n" {
		t.Fatalf("got %q", got)
	}
	if got := invoke(&rotated); got != "rotated-token|unset\n" {
		t.Fatalf("got %q", got)
	}
	if got := invoke(nil); got != "unset|unset\n" {
		t.Fatalf("got %q", got)
	}
}

// Hard-won: a4c6fe269: URL-specific repository auth outranked a host-level reset.
func TestCredentialFreeGitAndGhDoNotRecoverHostOrRefreshedCredentials(t *testing.T) {
	directory := t.TempDir()
	worker := filepath.Join(directory, "worker")
	home := filepath.Join(directory, "home")
	project := filepath.Join(directory, "project")
	for _, path := range []string{worker, home, project, filepath.Join(home, ".config/gh")} {
		runtimeRelayMkdirAll(t, path)
	}
	testSupportGit(project, "init", "-q")
	originalGit := "[user]\nname = Isolated Author\nemail = author@example.invalid\n[credential \"https://github.com\"]\nhelper = \"!f() { echo username=host-user; echo password=host-secret; }; f\"\n[http \"https://github.com\"]\nextraheader = Authorization: host-secret\n[url \"git@github.com:\"]\ninsteadOf = https://github.com/\n"
	runtimeRelayWriteFile(t, filepath.Join(home, ".gitconfig"), []byte(originalGit))
	originalGh := "github.com:\n    oauth_token: host-secret\n    user: host-user\n    git_protocol: https\n"
	runtimeRelayWriteFile(t, filepath.Join(home, ".config/gh/hosts.yml"), []byte(originalGh))
	testSupportGit(project, "config", "credential.https://github.com.helper", "!f() { echo username=repo-user; echo password=repo-secret; }; f")
	testSupportGit(project, "config", "credential.https://github.com/org/repo.helper", "!f() { echo username=repo-user; echo password=scoped-secret; }; f")
	testSupportGit(project, "config", "http.https://github.com/org/repo.extraheader", "Authorization: scoped-secret")
	loginHook := filepath.Join(home, "login-hook")
	runtimeRelayWriteFile(t, loginHook, []byte("export GH_TOKEN=login-secret GITHUB_TOKEN=login-secret\n"))
	runtimeRelayMkdirAll(t, filepath.Join(home, ".ssh"))
	sshConfig := filepath.Join(home, ".ssh/config")
	runtimeRelayWriteFile(t, sshConfig, []byte("Host github-work\n    HostName github.com\n    PubkeyAuthentication yes\n    ControlMaster auto\n    ControlPath /existing-authenticated-socket\nHost internal-work\n    HostName internal.example.invalid\n    PubkeyAuthentication yes\n"))
	parent := filepath.Join(directory, "parent-worker")
	runtimeRelayMkdirAll(t, parent)
	parentEnvironment := map[string]string{"PATH": os.Getenv("PATH")}
	runtimeRelayMust(t, ConfigureGithubCLI(parent, parentEnvironment, &home, nil, false))
	if _, err := core.WriteGithubToken(filepath.Join(parent, "github-token"), []byte("parent-secret")); err != nil {
		t.Fatal(err)
	}
	environment := map[string]string{
		"PATH":              parentEnvironment["PATH"],
		"HOME":              home,
		"XDG_CONFIG_HOME":   filepath.Join(home, ".config"),
		"GIT_CONFIG_GLOBAL": filepath.Join(home, ".gitconfig"),
		"BASH_ENV":          loginHook,
		"GH_TOKEN":          "stale-secret",
		"GITHUB_TOKEN":      "stale-secret",
	}
	if _, err := core.WriteGithubToken(filepath.Join(worker, "github-token"), []byte("stale-secret")); err != nil {
		t.Fatal(err)
	}
	for range 2 {
		runtimeRelayMust(t, ConfigureGithubCLI(worker, environment, &home, nil, true))
		if runtimeRelayExists(filepath.Join(worker, "github-token")) {
			t.Fatal("a credential-free worker kept its token file")
		}
		if _, ok := environment["GH_TOKEN"]; ok {
			t.Fatal("GH_TOKEN survived")
		}
		if _, ok := environment["GITHUB_TOKEN"]; ok {
			t.Fatal("GITHUB_TOKEN survived")
		}
		invoke := func(program string, args []string, input []byte) core.ProcessOutput {
			command := exec.Command(program, args...)
			command.Env = runtimeRelayEnvironList(environment)
			command.Dir = directory
			if lookup, ok := environment["PATH"]; ok && !strings.Contains(program, "/") {
				if resolved, err := runtimeRelayLookPath(program, lookup); err == nil {
					command.Path = resolved
					command.Err = nil
				}
			}
			return runtimeRelayRun(t, command, input)
		}
		gitOutput := invoke("git", []string{"-C", project, "credential", "fill"}, []byte("protocol=https\nhost=github.com\npath=org/repo\n\n"))
		if gitOutput.Status.Success() {
			t.Fatal("Git must not recover a host or repository credential")
		}
		gh := invoke("gh", []string{"auth", "token", "--hostname", "github.com"}, nil)
		if gh.Status.Success() {
			t.Fatal("gh must not recover the host login")
		}
		header := invoke("git", []string{"-C", project, "config", "--get-urlmatch", "http.extraheader", "https://github.com/org/repo"}, nil)
		if !header.Status.Success() {
			t.Fatalf("git config --get-urlmatch failed: %s", fromUTF8Lossy(header.Stderr))
		}
		if strings.TrimSpace(string(header.Stdout)) != "" {
			t.Fatalf("extraheader = %q", header.Stdout)
		}
		author := invoke("git", []string{"config", "user.name"}, nil)
		if got := strings.TrimSpace(string(author.Stdout)); got != "Isolated Author" {
			t.Fatalf("user.name = %q", got)
		}
		for _, alias := range []struct {
			name   string
			pubkey []string
		}{
			{"github-work", []string{"no", "false"}},
			{"internal-work", []string{"yes", "true"}},
		} {
			ssh := invoke(filepath.Join(worker, "bin/git-ssh"), []string{"-G", "-F", sshConfig, alias.name}, nil)
			if !ssh.Status.Success() {
				t.Fatalf("git-ssh -G failed: %s", fromUTF8Lossy(ssh.Stderr))
			}
			settings := string(ssh.Stdout)
			var setting string
			for _, line := range strings.Split(settings, "\n") {
				if strings.HasPrefix(line, "pubkeyauthentication ") {
					setting = line
					break
				}
			}
			if setting == "" {
				t.Fatal("ssh -G reported no pubkeyauthentication")
			}
			if !slices.Contains(alias.pubkey, strings.Fields(setting)[1]) {
				t.Fatalf("%s", setting)
			}
			if alias.name == "github-work" && strings.Contains(settings, "/existing-authenticated-socket") {
				t.Fatal("github-work kept the authenticated control socket")
			}
		}
		shell := invoke("bash", []string{"-c", "printf '%s|%s' \"${GH_TOKEN-unset}\" \"${GITHUB_TOKEN-unset}\""}, nil)
		if !shell.Status.Success() {
			t.Fatalf("bash failed: %s", fromUTF8Lossy(shell.Stderr))
		}
		if string(shell.Stdout) != "unset|unset" {
			t.Fatalf("shell saw %q", shell.Stdout)
		}
	}
	launch := launchConfig(worker)
	launch.NoGithubCredentials = true
	endpoint, err := credentialEndpoint(&launch)
	runtimeRelayMust(t, err)
	_, err = applyCredentialRequest(&endpoint, core.RelayRequestInstallGithubToken{Data: "bmV3LXNlY3JldA=="})
	if err == nil || !strings.Contains(err.Error(), "disabled") {
		t.Fatalf("installing a GitHub token into a credential-free worker: %v", err)
	}
	if runtimeRelayExists(filepath.Join(worker, "github-token")) {
		t.Fatal("the refused token was written")
	}
	if got := string(runtimeRelayReadFile(t, filepath.Join(home, ".gitconfig"))); got != originalGit {
		t.Fatalf(".gitconfig changed: %q", got)
	}
	if got := string(runtimeRelayReadFile(t, filepath.Join(home, ".config/gh/hosts.yml"))); got != originalGh {
		t.Fatalf("hosts.yml changed: %q", got)
	}
}

// runtimeRelayLookPath resolves program on searchPath, as Rust's Command
// does with the child's PATH when the parent environment is replaced.
func runtimeRelayLookPath(program, searchPath string) (string, error) {
	for _, directory := range filepath.SplitList(searchPath) {
		if directory == "" {
			continue
		}
		candidate := filepath.Join(directory, program)
		if info, err := os.Stat(candidate); err == nil && !info.IsDir() && info.Mode()&0o111 != 0 {
			return candidate, nil
		}
	}
	return "", fmt.Errorf("%s not found on %s", program, searchPath)
}

func TestSlopcopWrapperExportsEachLiveLogin(t *testing.T) {
	worker := t.TempDir()
	home := t.TempDir()
	real := t.TempDir()
	realBin := filepath.Join(real, "bin")
	runtimeRelayMust(t, os.Mkdir(realBin, 0o755))
	core.InstallFakeCommand(realBin, "slopcop", "#!/bin/sh\nprintf '%s|%s|%s\\n' \"${SLOPCOP_TOKEN-unset}\" \"${SLOPCOP_SERVICE-unset}\" \"$*\"\n")
	environment := map[string]string{"PATH": realBin + ":/usr/bin:/bin"}
	runtimeRelayMust(t, ConfigureGithubCLI(worker, environment, nil, nil, false))
	loginPath := filepath.Join(home, core.LoginFile)
	runtimeRelayMust(t, ConfigureSlopcopCLI(worker, loginPath))

	invoke := func() string {
		command := exec.Command(filepath.Join(worker, "bin/slopcop"), "check", "--format", "json")
		command.Env = []string{"PATH=" + environment["PATH"], "SLOPCOP_TOKEN=session-token"}
		output := runtimeRelayRun(t, command, nil)
		if !output.Status.Success() {
			t.Fatalf("%s", fromUTF8Lossy(output.Stderr))
		}
		return string(output.Stdout)
	}

	// With no synced login, the session's own environment applies.
	if got := invoke(); got != "session-token|unset|check --format json\n" {
		t.Fatalf("got %q", got)
	}
	// Credential sync replaces the file under a running session; every
	// invocation reads the current one.
	service := "https://slopcop.example"
	first, err := core.NewSlopcopLogin("first-token", &service)
	runtimeRelayMust(t, err)
	rotated, err := core.NewSlopcopLogin("rotated-token", nil)
	runtimeRelayMust(t, err)
	for _, step := range []struct {
		login    core.SlopcopLogin
		expected string
	}{
		{first, "first-token|https://slopcop.example|check --format json\n"},
		{rotated, "rotated-token|unset|check --format json\n"},
	} {
		if _, err := core.WriteLogin(loginPath, step.login.Encode()); err != nil {
			t.Fatal(err)
		}
		if got := invoke(); got != step.expected {
			t.Fatalf("got %q, want %q", got, step.expected)
		}
	}
	runtimeRelayMust(t, core.RemoveLogin(loginPath))
	if got := invoke(); got != "session-token|unset|check --format json\n" {
		t.Fatalf("got %q", got)
	}
}

// Hard-won: 50963cae: a harness scrubbed credential-shaped Git config names and broke every Git command.
func TestGitHelpersSurviveHarnessLoginShellsAndCredentialShapedNameScrubs(t *testing.T) {
	worker := t.TempDir()
	home := t.TempDir()
	real := t.TempDir()
	realBin := filepath.Join(real, "bin")
	runtimeRelayMust(t, os.Mkdir(realBin, 0o755))
	core.InstallFakeCommand(realBin, "gh", `#!/bin/sh
if [ "${1-}" = auth ] && [ "${2-}" = git-credential ]; then
    cat >/dev/null
    if [ "${3-}" = get ]; then
        printf 'username=x-access-token\npassword=%s\n' "${GH_TOKEN-unset}"
    fi
else
    printf '%s|%s\n' "${GH_TOKEN-unset}" "${GITHUB_TOKEN-unset}"
fi
`)

	originalBashEnv := filepath.Join(home, "original-bash-env")
	runtimeRelayWriteFile(t, originalBashEnv, []byte("export ORIGINAL_BASH_ENV=preserved\n"))
	// The session user's own global configuration must keep working, and its
	// credential helper must lose to the worker's absolute wrapper.
	runtimeRelayWriteFile(t, filepath.Join(home, ".gitconfig"), []byte("[user]\n\tname = Home User\n[credential \"https://github.com\"]\n\thelper = decoy\n"))
	runtimeRelayWriteFile(t, filepath.Join(home, ".bash_profile"), []byte(fmt.Sprintf("PATH=%s:%s\nexport PATH\n", realBin, "/usr/bin:/bin")))

	environment := map[string]string{
		"PATH":               realBin + ":/usr/bin:/bin",
		"HOME":               home,
		"BASH_ENV":           originalBashEnv,
		"GIT_CONFIG_COUNT":   "1",
		"GIT_CONFIG_KEY_0":   "user.email",
		"GIT_CONFIG_VALUE_0": "harness@example.test",
	}
	runtimeRelayMust(t, ConfigureGithubCLI(worker, environment, nil, nil, false))
	configuredOnce := maps.Clone(environment)
	runtimeRelayMust(t, ConfigureGithubCLI(worker, environment, nil, nil, false))
	if !maps.Equal(environment, configuredOnce) {
		t.Fatalf("configuring twice changed the environment: %v != %v", environment, configuredOnce)
	}

	// Git settings reach the harness through one worker-owned file, not
	// through a count whose credential-shaped key names a harness may drop.
	gitConfigPath := filepath.Join(worker, "gitconfig")
	if got, ok := environment["GIT_CONFIG_GLOBAL"]; !ok || got != gitConfigPath {
		t.Fatalf("GIT_CONFIG_GLOBAL = %q, want %q", got, gitConfigPath)
	}
	for name := range environment {
		if name == "GIT_CONFIG_COUNT" || strings.HasPrefix(name, "GIT_CONFIG_KEY_") || strings.HasPrefix(name, "GIT_CONFIG_VALUE_") {
			t.Fatalf("%v", environment)
		}
	}
	if _, err := core.WriteGithubToken(filepath.Join(worker, "github-token"), []byte("synchronized-test-token")); err != nil {
		t.Fatal(err)
	}

	direct := exec.Command("/bin/bash", "-lc", "printf '%s|%s\\n' \"$(command -v gh)\" \"${ORIGINAL_BASH_ENV-unset}\"; gh auth status")
	direct.Env = runtimeRelayEnvironList(environment)
	output := runtimeRelayRun(t, direct, nil)
	if !output.Status.Success() {
		t.Fatalf("%s", fromUTF8Lossy(output.Stderr))
	}
	if want := fmt.Sprintf("%s|preserved\nsynchronized-test-token|unset\n", filepath.Join(worker, "bin/gh")); string(output.Stdout) != want {
		t.Fatalf("got %q, want %q", output.Stdout, want)
	}

	// Run outside any repository: repository-local settings would otherwise
	// answer for the configuration under test.
	runWith := func(environment map[string]string, script string) string {
		command := exec.Command("/bin/bash", "-lc", script)
		command.Dir = home
		command.Env = runtimeRelayEnvironList(environment)
		output := runtimeRelayRun(t, command, nil)
		if !output.Status.Success() {
			t.Fatalf("%s", fromUTF8Lossy(output.Stderr))
		}
		return string(output.Stdout)
	}
	const identity = "printf '%s|%s\\n' \"$(git config --get user.name)\" \"$(git config --get user.email)\""
	const credentials = "printf 'protocol=https\\nhost=github.com\\n\\n' | git credential fill"

	// The included home file supplies the name, the inherited entry supplies
	// the email, and the worker's wrapper outranks the home helper.
	if got := runWith(environment, identity); got != "Home User|harness@example.test\n" {
		t.Fatalf("identity = %q", got)
	}
	filled := runWith(environment, credentials)
	if !strings.Contains(filled, "username=x-access-token\n") || !strings.Contains(filled, "password=synchronized-test-token\n") {
		t.Fatalf("%s", filled)
	}

	// A harness may spawn each tool with the parent environment scrubbed of
	// every name matching `KEY|PASSWORD|SECRET|TOKEN`. That drops
	// `GIT_CONFIG_KEY_*` while keeping `GIT_CONFIG_COUNT`, which left every git
	// command in the session failing with "missing config key GIT_CONFIG_KEY_0".
	scrubbed := map[string]string{}
	for name, value := range environment {
		upper := strings.ToUpper(name)
		if !slices.ContainsFunc([]string{"KEY", "PASSWORD", "SECRET", "TOKEN"}, func(shape string) bool {
			return strings.Contains(upper, shape)
		}) {
			scrubbed[name] = value
		}
	}
	if got := runWith(scrubbed, identity); got != "Home User|harness@example.test\n" {
		t.Fatalf("scrubbed identity = %q", got)
	}
	filled = runWith(scrubbed, credentials)
	if !strings.Contains(filled, "username=x-access-token\n") || !strings.Contains(filled, "password=synchronized-test-token\n") {
		t.Fatalf("%s", filled)
	}
}

// A worker installs a tree sent in either archive format: a current
// controller sends it compressed, and the uncompressed format is the one
// every earlier release wrote.
func TestInstallSkillsReplacesTheSessionTreeAndReportsTheNewState(t *testing.T) {
	canonical := t.TempDir()
	runtimeRelayMkdirAll(t, filepath.Join(canonical, "skills/review"))
	runtimeRelayWriteFile(t, filepath.Join(canonical, "skills/review/SKILL.md"), []byte("v1"))
	archive, err := core.CollectSkills(core.HarnessKindCodex, canonical)
	runtimeRelayMust(t, err)

	for _, format := range []core.SkillsArchiveFormat{core.SkillsArchiveFormatPlain, core.SkillsArchiveFormatGzip} {
		home := t.TempDir()
		runtimeRelayMkdirAll(t, filepath.Join(home, "skills/stale"))
		runtimeRelayWriteFile(t, filepath.Join(home, "skills/stale/SKILL.md"), []byte("old"))
		config := launchConfig(home)
		endpoint, err := credentialEndpoint(&config)
		runtimeRelayMust(t, err)

		payload, err := applyCredentialRequest(&endpoint, skillsInstallRequest(archive.Encode(format)))
		runtimeRelayMust(t, err)
		state := skillsStateOf(t, payload)
		if state != archive.State() {
			t.Fatalf("%v: state = %#v, want %#v", format, state, archive.State())
		}
		if got := runtimeRelayReadFile(t, filepath.Join(home, "skills/review/SKILL.md")); string(got) != "v1" {
			t.Fatalf("%v: SKILL.md = %q", format, got)
		}
		if runtimeRelayExists(filepath.Join(home, "skills/stale")) {
			t.Fatalf("%v: the stale skill survived", format)
		}

		empty := core.SkillsArchive{}
		payload, err = applyCredentialRequest(&endpoint, skillsInstallRequest(empty.Encode(format)))
		runtimeRelayMust(t, err)
		state = skillsStateOf(t, payload)
		if state.Present {
			t.Fatalf("%v: an empty archive reported a present tree", format)
		}
		if runtimeRelayExists(filepath.Join(home, "skills")) {
			t.Fatalf("%v: an empty archive left the skills tree", format)
		}
	}
}

func TestInstallSkillsRejectsGarbageAndLeavesTheTreeUntouched(t *testing.T) {
	home := t.TempDir()
	runtimeRelayMkdirAll(t, filepath.Join(home, "skills"))
	runtimeRelayWriteFile(t, filepath.Join(home, "skills/keep.md"), []byte("keep"))
	config := launchConfig(home)
	endpoint, err := credentialEndpoint(&config)
	runtimeRelayMust(t, err)

	_, err = applyCredentialRequest(&endpoint, skillsInstallRequest([]byte("garbage-with-enough-length")))
	if err == nil || !strings.Contains(err.Error(), "magic") {
		t.Fatalf("%v", err)
	}
	if got := runtimeRelayReadFile(t, filepath.Join(home, "skills/keep.md")); string(got) != "keep" {
		t.Fatalf("keep.md = %q", got)
	}
}

// Hard-won: #1385: isolated Bedrock workers never received the host AWS credentials.
// Hard-won: #1390: worker upgrades synced credential JSON without its process-provider configuration.
func TestCredentialExchangeStaysOnTheConnectionAndOutOfRelayState(t *testing.T) {
	temp := t.TempDir()
	relayRoot := filepath.Join(temp, "relay")
	relay := runtimeRelayCell(t, relayRoot, "1.0.0")
	config := launchConfig(temp)
	endpoint, err := credentialEndpoint(&config)
	runtimeRelayMust(t, err)
	wake := newDispatchWakeChannel()
	server, client := runtimeRelayPair(t)
	fatal, _ := fatalReports()
	serverTask := core.Spawn(func() error {
		return serveClient(context.Background(), server, relay, wake, credentialEndpointResult{endpoint: endpoint}, nil, fatal)
	})
	lines := bufio.NewReader(client)
	credentialBytes := codexCredentials("2026-08-05T02:51:00.864587231Z")
	request := core.RelayRequestEnvelope{
		RequestID:       "install-credentials",
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         installRequest(credentialBytes),
	}
	response := runtimeRelayExchange(t, lines, client, request)
	state, ok := runtimeRelayOk(t, response, "credential install failed").(core.RelayResponsePayloadCredentialState)
	if !ok {
		t.Fatalf("credential install failed: %#v", response.Body)
	}
	if !state.Present {
		t.Fatal("installed credentials are absent")
	}
	if state.Fingerprint != core.CredentialFingerprint(credentialBytes) {
		t.Fatalf("fingerprint = %q", state.Fingerprint)
	}
	if state.FreshnessEpochMs == nil || *state.FreshnessEpochMs != 1_785_898_260_864 {
		t.Fatalf("freshness = %v", state.FreshnessEpochMs)
	}
	if got := runtimeRelayReadFile(t, filepath.Join(temp, "auth.json")); !bytes.Equal(got, credentialBytes) {
		t.Fatalf("auth.json = %q", got)
	}

	response = runtimeRelayExchange(t, lines, client, core.RelayRequestEnvelope{
		RequestID:       "read-credentials",
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         core.RelayRequestReadCredentials{},
	})
	read, ok := runtimeRelayOk(t, response, "credential read failed").(core.RelayResponsePayloadCredentials)
	if !ok {
		t.Fatalf("credential read failed: %#v", response.Body)
	}
	decoded, err := base64.StdEncoding.DecodeString(read.Data)
	runtimeRelayMust(t, err)
	if !bytes.Equal(decoded, credentialBytes) {
		t.Fatalf("read credentials = %q", decoded)
	}

	// Exercise the connection-only AWS route with enough data to cross a
	// pipe buffer, then renew it without restarting the running worker.
	for _, generation := range []string{"initial", "renewed"} {
		export, err := json.Marshal(map[string]any{
			"Version":         1,
			"AccessKeyId":     "key-" + generation,
			"SecretAccessKey": "secret-" + generation,
			"SessionToken":    strings.Repeat("s", 70_000),
			"Expiration":      "2099-01-01T00:00:00Z",
		})
		runtimeRelayMust(t, err)
		credentials, err := core.AwsCredentialsFromExport("bedrock", export)
		runtimeRelayMust(t, err)
		awsBytes, err := credentials.Encode()
		runtimeRelayMust(t, err)
		for _, request := range []core.RelayRequest{
			core.RelayRequestInstallAwsCredentials{Data: base64.StdEncoding.EncodeToString(awsBytes), Scope: nil},
			core.RelayRequestAwsCredentialsState{},
		} {
			response := runtimeRelayExchange(t, lines, client, core.RelayRequestEnvelope{
				RequestID:       "aws-" + generation,
				ProtocolVersion: core.RelayProtocolVersion,
				Request:         request,
			})
			state, ok := runtimeRelayOk(t, response, "AWS credential request failed").(core.RelayResponsePayloadCredentialState)
			if !ok {
				t.Fatalf("AWS credential request failed: %#v", response.Body)
			}
			if !state.Present {
				t.Fatal("AWS credentials are absent")
			}
			if state.Fingerprint != core.CredentialFingerprint(awsBytes) {
				t.Fatalf("AWS fingerprint = %q", state.Fingerprint)
			}
		}
		path := filepath.Join(temp, core.AwsCredentialsSharedCredentialsFile)
		if got := runtimeRelayReadFile(t, path); !bytes.Equal(got, awsBytes) {
			t.Fatal("the shared credentials file does not hold the installed credentials")
		}
		if mode := runtimeRelayFileMode(t, path); mode != 0o600 {
			t.Fatalf("shared credentials mode = %o", mode)
		}

		// An upgraded worker can have the same credentials as the host but
		// lack the files its new launch environment tells the SDK to read.
		// Both missing and inconsistent provider files must trigger repair.
		for _, step := range []struct {
			name        string
			replacement *string
		}{
			{"aws-config", nil},
			{"aws-empty-credentials", nil},
			{"aws-config", runtimeRelayString("[profile wrong]\n")},
			{"aws-empty-credentials", runtimeRelayString("[default]\n")},
		} {
			path := filepath.Join(temp, step.name)
			if step.replacement != nil {
				runtimeRelayWriteFile(t, path, []byte(*step.replacement))
			} else {
				runtimeRelayMust(t, os.Remove(path))
			}
			state := delegationExchange(t, lines, client, core.RelayRequestAwsCredentialsState{})
			if credential, ok := state.(core.RelayResponsePayloadCredentialState); !ok || credential.Present {
				t.Fatalf("%s: a damaged provider configuration must read as absent: %#v", step.name, state)
			}
			installed := delegationExchange(t, lines, client, core.RelayRequestInstallAwsCredentials{
				Data:  base64.StdEncoding.EncodeToString(awsBytes),
				Scope: nil,
			})
			state = delegationExchange(t, lines, client, core.RelayRequestAwsCredentialsState{})
			if !reflect.DeepEqual(installed, state) {
				t.Fatalf("install reported %#v, state %#v", installed, state)
			}
			if credential, ok := state.(core.RelayResponsePayloadCredentialState); !ok || !credential.Present {
				t.Fatalf("%s: repaired credentials are absent: %#v", step.name, state)
			}
			for _, name := range []string{"aws-config", "aws-empty-credentials"} {
				if mode := runtimeRelayFileMode(t, filepath.Join(temp, name)); mode != 0o600 {
					t.Fatalf("%s mode = %o", name, mode)
				}
			}
			configuration, err := credentials.Configuration(temp)
			runtimeRelayMust(t, err)
			if got := runtimeRelayReadFile(t, filepath.Join(temp, "aws-config")); !bytes.Equal(got, configuration) {
				t.Fatalf("aws-config = %q, want %q", got, configuration)
			}
			if got := runtimeRelayReadFile(t, filepath.Join(temp, "aws-empty-credentials")); len(got) != 0 {
				t.Fatalf("aws-empty-credentials = %q", got)
			}
		}
	}

	// Credentials Mjolnir issued carry a scope the worker keeps beside the
	// secret and reports; it describes only the secret it was written with.
	scope := core.AwsCredentialScope{
		RoleArn:                  "arn:aws:iam::123456789012:role/mj-bedrock",
		RoleSessionName:          "mj-0123456789abcdef0123456789abcdef",
		SessionPolicyFingerprint: "policy",
		SourceIdentity:           nil,
		ExpiresAtEpochMillis:     4_070_908_800_000,
		Tag:                      "tag",
	}
	scopedCredentials := func(key string, scope *core.AwsCredentialScope) string {
		export, err := json.Marshal(map[string]any{
			"Version": 1, "AccessKeyId": key, "SecretAccessKey": "secret",
			"SessionToken": "token", "Expiration": "2099-01-01T00:00:00Z",
		})
		runtimeRelayMust(t, err)
		credentials, err := core.AwsCredentialsFromExport("bedrock", export)
		runtimeRelayMust(t, err)
		var recorded *core.AwsCredentialScope
		if scope != nil {
			copied := *scope
			recorded = &copied
		}
		encoded, err := credentials.WithScope(recorded).Encode()
		runtimeRelayMust(t, err)
		return base64.StdEncoding.EncodeToString(encoded)
	}
	for _, step := range []struct {
		key      string
		recorded *core.AwsCredentialScope
		expected *core.AwsCredentialScope
	}{
		{"issued", &scope, &scope},
		{"host-own", nil, nil},
	} {
		delegationExchange(t, lines, client, core.RelayRequestInstallAwsCredentials{
			Data:  scopedCredentials(step.key, step.recorded),
			Scope: step.recorded,
		})
		reported := delegationExchange(t, lines, client, core.RelayRequestAwsCredentialsScope{})
		scoped, ok := reported.(core.RelayResponsePayloadAwsCredentialsScope)
		if !ok || !reflect.DeepEqual(scoped.Scope, step.expected) {
			t.Fatalf("%s: %#v", step.key, reported)
		}
	}
	// A record never vouches for a different secret.
	delegationExchange(t, lines, client, core.RelayRequestInstallAwsCredentials{
		Data:  scopedCredentials("issued", &scope),
		Scope: &scope,
	})
	secret := filepath.Join(temp, core.AwsCredentialsSharedCredentialsFile)
	runtimeRelayWriteFile(t, secret, []byte(strings.ReplaceAll(string(runtimeRelayReadFile(t, secret)), "issued", "swapped")))
	reported := delegationExchange(t, lines, client, core.RelayRequestAwsCredentialsScope{})
	if scoped, ok := reported.(core.RelayResponsePayloadAwsCredentialsScope); !ok || scoped.Scope != nil {
		t.Fatalf("a swapped secret kept its scope: %#v", reported)
	}

	// Withdrawing the credentials deletes the secret file, reports them absent
	// and leaves the way open to install again.
	removed := delegationExchange(t, lines, client, core.RelayRequestRemoveAwsCredentials{})
	if state, ok := removed.(core.RelayResponsePayloadCredentialState); !ok || state.Present {
		t.Fatalf("removed = %#v", removed)
	}
	if runtimeRelayExists(filepath.Join(temp, core.AwsCredentialsSharedCredentialsFile)) {
		t.Fatal("the secret file survived removal")
	}
	again := delegationExchange(t, lines, client, core.RelayRequestRemoveAwsCredentials{})
	if state, ok := again.(core.RelayResponsePayloadCredentialState); !ok || state.Present {
		t.Fatalf("again = %#v", again)
	}
	// The profile the session was staged with outlives the credentials: the
	// controller asks the worker for it, and the worker still knows.
	staged := delegationExchange(t, lines, client, core.RelayRequestAwsCredentialsScope{})
	if scoped, ok := staged.(core.RelayResponsePayloadAwsCredentialsScope); !ok || scoped.Scope != nil || scoped.Profile == nil || *scoped.Profile != "bedrock" {
		t.Fatalf("%#v", staged)
	}

	// The host's slopcop login takes the same connection-only route.
	service := "https://slopcop.example"
	login, err := core.NewSlopcopLogin("slopcop-secret", &service)
	runtimeRelayMust(t, err)
	installed := delegationExchange(t, lines, client, core.RelayRequestInstallSlopcopLogin{
		Data: base64.StdEncoding.EncodeToString(login.Encode()),
	})
	slopcopState := delegationExchange(t, lines, client, core.RelayRequestSlopcopLoginState{})
	if !reflect.DeepEqual(installed, slopcopState) {
		t.Fatalf("install reported %#v, state %#v", installed, slopcopState)
	}
	if want := (core.RelayResponsePayloadCredentialState{
		Present:          true,
		Fingerprint:      login.Snapshot().Fingerprint,
		FreshnessEpochMs: nil,
	}); !reflect.DeepEqual(slopcopState, want) {
		t.Fatalf("slopcop state = %#v, want %#v", slopcopState, want)
	}
	loginPath := filepath.Join(temp, core.LoginFile)
	if got := runtimeRelayReadFile(t, loginPath); !bytes.Equal(got, login.Encode()) {
		t.Fatalf("login file = %q", got)
	}
	if mode := runtimeRelayFileMode(t, loginPath); mode != 0o600 {
		t.Fatalf("login mode = %o", mode)
	}
	removed = delegationExchange(t, lines, client, core.RelayRequestRemoveSlopcopLogin{})
	if state, ok := removed.(core.RelayResponsePayloadCredentialState); !ok || state.Present {
		t.Fatalf("removed = %#v", removed)
	}
	if runtimeRelayExists(loginPath) {
		t.Fatal("the slopcop login survived removal")
	}

	{
		locked := relay.Lock()
		if locked.LatestOrdinal() != 0 {
			t.Fatalf("latest ordinal = %d", locked.LatestOrdinal())
		}
		events, err := locked.EventsAfter(0, core.RelayEventGenesisDigest)
		runtimeRelayMust(t, err)
		if len(events) != 0 {
			t.Fatalf("credential traffic was journaled: %#v", events)
		}
		persisted := string(runtimeRelayReadFile(t, filepath.Join(relayRoot, "relay-state.json")))
		for _, secret := range []string{request.RequestID, "aws-initial", "secret-renewed", "slopcop-secret"} {
			if strings.Contains(persisted, secret) {
				t.Fatalf("relay state names %q", secret)
			}
		}
		locked.Unlock()
	}

	client.Close()
	runtimeRelayJoinOk(t, serverTask, "serve the credential connection")
}

func runtimeRelayString(value string) *string { return &value }

func TestProtocolV1CannotRespondToElicitation(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, filepath.Join(temp, "relay"), "1.0.0")
	wake := newDispatchWakeChannel()
	commands := newCommandChannel(1)
	server, client := runtimeRelayPair(t)
	fatal, _ := fatalReports()
	serverTask := core.Spawn(func() error {
		return serveClient(context.Background(), server, relay, wake, testCredentials(), commands, fatal)
	})
	lines := bufio.NewReader(client)
	response := runtimeRelayExchange(t, lines, client, core.RelayRequestEnvelope{
		RequestID:       "elicit-v1",
		ProtocolVersion: 1,
		Request: core.RelayRequestRespondElicitation{
			ElicitationID: "form-1",
			Response:      core.ElicitationResponse{Action: core.ElicitationResponseCancel},
		},
	})
	body, ok := response.Body.(core.RelayResponseBodyError)
	if !ok {
		t.Fatalf("protocol v1 must not answer elicitations, got %#v", response)
	}
	if body.Error.Code != core.RelayErrorCodeIncompatibleProtocol {
		t.Fatalf("error code = %q", body.Error.Code)
	}
	select {
	case command := <-commands.receiver():
		t.Fatalf("a v1 elicitation answer reached the ACP runtime: %#v", command)
	default:
	}

	client.Close()
	runtimeRelayJoinOk(t, serverTask, "serve the elicitation connection")
}

func TestIncompatibleProtocolCannotReadOrMutateCredentials(t *testing.T) {
	home := t.TempDir()
	relay := runtimeRelayCell(t, filepath.Join(home, "relay"), "1.0.0")
	config := launchConfig(home)
	endpoint, err := credentialEndpoint(&config)
	runtimeRelayMust(t, err)
	original := codexCredentials("2026-08-05T02:51:00Z")
	runtimeRelayMust(t, core.WriteCredentialFile(endpoint.Harness, endpoint.Marker, original))
	replacement := codexCredentials("2026-08-06T02:51:00Z")
	wake := newDispatchWakeChannel()
	server, client := runtimeRelayPair(t)
	fatal, _ := fatalReports()
	serverTask := core.Spawn(func() error {
		return serveClient(context.Background(), server, relay, wake, credentialEndpointResult{endpoint: endpoint}, nil, fatal)
	})
	lines := bufio.NewReader(client)

	for _, step := range []struct {
		requestID string
		request   core.RelayRequest
	}{
		{"read-with-v0", core.RelayRequestReadCredentials{}},
		{"install-with-v0", installRequest(replacement)},
	} {
		response := runtimeRelayExchange(t, lines, client, core.RelayRequestEnvelope{
			RequestID:       step.requestID,
			ProtocolVersion: 0,
			Request:         step.request,
		})
		if response.ProtocolVersion != 0 {
			t.Fatalf("protocol version = %d", response.ProtocolVersion)
		}
		body, ok := response.Body.(core.RelayResponseBodyError)
		if !ok || body.Error.Code != core.RelayErrorCodeIncompatibleProtocol || body.Error.Retryable {
			t.Fatalf("%s: %#v", step.requestID, response.Body)
		}
	}

	if got := runtimeRelayReadFile(t, filepath.Join(home, "auth.json")); !bytes.Equal(got, original) {
		t.Fatalf("auth.json = %q", got)
	}
	client.Close()
	runtimeRelayJoinOk(t, serverTask, "serve the credential connection")
}

func TestInstalledCredentialsAreOwnerOnly(t *testing.T) {
	home := t.TempDir()
	config := launchConfig(home)
	endpoint, err := credentialEndpoint(&config)
	runtimeRelayMust(t, err)
	_, err = credentialRequest(context.Background(), endpoint, installRequest(codexCredentials("2026-08-05T02:51:00Z")))
	runtimeRelayMust(t, err)

	if mode := runtimeRelayFileMode(t, filepath.Join(home, "auth.json")); mode != 0o600 {
		t.Fatalf("mode = %o", mode)
	}
}

func TestInstallingKimiCredentialsUsesItsFixedNestedMarker(t *testing.T) {
	// A configuration persisted before the controller stated the marker, and
	// one stating it the way the controller does now: both install where Kimi
	// reads its login.
	for _, stated := range []*string{nil, runtimeRelayString("credentials/kimi-code.json")} {
		home := t.TempDir()
		config := launchConfig(home)
		config.Harness = core.HarnessKindKimi
		config.AuthenticationMarker = stated
		config.Environment = map[string]string{"KIMI_CODE_HOME": home}
		endpoint, err := credentialEndpoint(&config)
		runtimeRelayMust(t, err)
		credentialBytes, err := json.Marshal(map[string]any{
			"access_token": "access",
			"expires_at":   1_755_000_000,
		})
		runtimeRelayMust(t, err)

		_, err = credentialRequest(context.Background(), endpoint, installRequest(credentialBytes))
		runtimeRelayMust(t, err)

		if got := runtimeRelayReadFile(t, filepath.Join(home, "credentials/kimi-code.json")); !bytes.Equal(got, credentialBytes) {
			t.Fatalf("%v: kimi-code.json = %q", stated, got)
		}
		if runtimeRelayExists(filepath.Join(home, "kimi-code.json")) {
			t.Fatalf("%v: a top-level kimi-code.json was written", stated)
		}
	}
}

// Hard-won: #1485: freshness ordering rejected authoritative OpenCode updates and empty exports.
func TestOpencodeCredentialExportsRemainAuthoritativeAcrossUpdatesAndDeletion(t *testing.T) {
	home := t.TempDir()
	config := launchConfig(home)
	config.Harness = core.HarnessKindOpenCode
	endpoint, err := credentialEndpoint(&config)
	runtimeRelayMust(t, err)

	for _, export := range [][]byte{
		[]byte(`[{"provider":"openai","type":"oauth","access":"old"}]`),
		[]byte(`[{"provider":"openai","type":"oauth","access":"new"}]`),
		[]byte("[]"),
	} {
		payload, err := credentialRequest(context.Background(), endpoint, installRequest(export))
		runtimeRelayMust(t, err)
		state, ok := payload.(core.RelayResponsePayloadCredentialState)
		if !ok {
			t.Fatalf("unexpected OpenCode credential response: %#v", payload)
		}
		if !state.Present {
			t.Fatal("OpenCode credentials are absent")
		}
		if state.Fingerprint != core.CredentialFingerprint(export) {
			t.Fatalf("fingerprint = %q", state.Fingerprint)
		}
		if got := runtimeRelayReadFile(t, endpoint.Marker); !bytes.Equal(got, export) {
			t.Fatalf("marker = %q, want %q", got, export)
		}
	}
}

func TestAbsentReadsAndInvalidInstallsAreRefused(t *testing.T) {
	home := t.TempDir()
	config := launchConfig(home)
	endpoint, err := credentialEndpoint(&config)
	runtimeRelayMust(t, err)

	_, err = applyCredentialRequest(&endpoint, core.RelayRequestReadCredentials{})
	if err == nil || !strings.Contains(err.Error(), "no") {
		t.Fatalf("%v", err)
	}

	_, err = credentialRequest(context.Background(), endpoint, installRequest([]byte("not json")))
	if err == nil || !strings.Contains(err.Error(), "JSON") {
		t.Fatalf("%v", err)
	}

	oversized := bytes.Repeat([]byte{'a'}, core.MaxCredentialBytes+1)
	_, err = credentialRequest(context.Background(), endpoint, installRequest(oversized))
	if err == nil || !strings.Contains(err.Error(), "limit") {
		t.Fatalf("%v", err)
	}
}

func TestInstallingOverASymlinkLeavesTheLinkTargetUntouched(t *testing.T) {
	home := t.TempDir()
	elsewhere := filepath.Join(home, "stolen.json")
	runtimeRelayWriteFile(t, elsewhere, []byte("{}"))
	runtimeRelayMust(t, os.Symlink(elsewhere, filepath.Join(home, "auth.json")))
	config := launchConfig(home)
	endpoint, err := credentialEndpoint(&config)
	runtimeRelayMust(t, err)

	_, err = credentialRequest(context.Background(), endpoint, installRequest(codexCredentials("2026-08-05T02:51:00Z")))

	if err == nil || !strings.Contains(err.Error(), "symbolic link") {
		t.Fatalf("%v", err)
	}
	if got := runtimeRelayReadFile(t, elsewhere); string(got) != "{}" {
		t.Fatalf("the link target changed: %q", got)
	}
}

func TestMuseRelativeRootsResolveBeforeCredentialAndHistoryAccess(t *testing.T) {
	config := launchConfig("unused")
	config.Harness = core.HarnessKindMuse
	config.Environment = map[string]string{}
	config.HarnessHome = "profiles/session/muse"
	config.Harness.ConfigureHomeEnvironment("profiles/session/muse", config.Environment)
	resolveRelativeHarnessHome(&config, "/home/remote")
	endpoint, err := credentialEndpoint(&config)
	runtimeRelayMust(t, err)
	if endpoint.Home != "/home/remote/profiles/session/muse" {
		t.Fatalf("home = %q", endpoint.Home)
	}
	if endpoint.Marker != filepath.Join(endpoint.Home, "auth.json") {
		t.Fatalf("marker = %q", endpoint.Marker)
	}
	if got := config.Environment["XDG_DATA_HOME"]; got != "/home/remote/profiles/session/muse/.data" {
		t.Fatalf("XDG_DATA_HOME = %q", got)
	}
}

func TestAStatedHarnessHomeServesCredentialsWithoutAHomeVariable(t *testing.T) {
	config := launchConfig("/profile")
	config.Harness = core.HarnessKindClaude
	config.HarnessHome = "/home/user/.claude"
	// The stated home is authoritative even for a launch environment that
	// carries no home variable, as a Claude session on macOS had before every
	// local session ran from a staged home.
	config.Environment = map[string]string{}

	endpoint, err := credentialEndpoint(&config)
	runtimeRelayMust(t, err)

	if endpoint.Home != "/home/user/.claude" {
		t.Fatalf("home = %q", endpoint.Home)
	}
	if endpoint.Marker != filepath.Join(endpoint.Home, ".credentials.json") {
		t.Fatalf("marker = %q", endpoint.Marker)
	}
}

// runtimeRelayJSONObject is serde_json::to_value of value as an object.
func runtimeRelayJSONObject(t *testing.T, value any) map[string]any {
	t.Helper()
	encoded, err := json.Marshal(value)
	runtimeRelayMust(t, err)
	var object map[string]any
	runtimeRelayMust(t, json.Unmarshal(encoded, &object))
	return object
}

// runtimeRelayFromJSONObject is serde_json::from_value of object into T.
func runtimeRelayFromJSONObject[T any](t *testing.T, object map[string]any) (T, error) {
	t.Helper()
	var value T
	encoded, err := json.Marshal(object)
	runtimeRelayMust(t, err)
	err = json.Unmarshal(encoded, &value)
	return value, err
}

// runtimeRelayCloneLaunch is WorkerLaunchConfig::clone for the maps a test
// changes: Go copies share them.
func runtimeRelayCloneLaunch(config core.WorkerLaunchConfig) core.WorkerLaunchConfig {
	clone := config
	clone.Environment = maps.Clone(config.Environment)
	clone.TargetEnvironment = maps.Clone(config.TargetEnvironment)
	clone.BridgeArgs = slices.Clone(config.BridgeArgs)
	clone.ExcludedEnvironment = slices.Clone(config.ExcludedEnvironment)
	clone.AdditionalDirectories = slices.Clone(config.AdditionalDirectories)
	clone.InitialCommands = slices.Clone(config.InitialCommands)
	return clone
}

func TestLaunchWiresRequireTheNewBaselineShape(t *testing.T) {
	launch := launchConfig("profile-home")
	retired := runtimeRelayJSONObject(t, launch)
	retired["recover_native_session"] = true
	if _, err := runtimeRelayFromJSONObject[core.WorkerLaunchConfig](t, retired); err == nil {
		t.Fatal("a retired field was accepted")
	}

	incomplete := runtimeRelayJSONObject(t, launch)
	delete(incomplete, "bridge_args")
	if _, err := runtimeRelayFromJSONObject[core.WorkerLaunchConfig](t, incomplete); err == nil {
		t.Fatal("a launch without bridge_args was accepted")
	}

	missingPolicy := runtimeRelayJSONObject(t, launch)
	delete(missingPolicy, "execution_policy")
	if _, err := runtimeRelayFromJSONObject[core.WorkerLaunchConfig](t, missingPolicy); err == nil {
		t.Fatal("a launch without execution_policy was accepted")
	}

	legacyPolicyObject := runtimeRelayJSONObject(t, launch)
	delete(legacyPolicyObject, "execution_policy")
	legacyPolicyObject["force_unrestricted_mode"] = true
	legacyPolicy, err := runtimeRelayFromJSONObject[core.WorkerLaunchConfig](t, legacyPolicyObject)
	runtimeRelayMust(t, err)
	if legacyPolicy.ExecutionPolicy != core.ExecutionPolicyUnconstrained {
		t.Fatalf("execution policy = %v", legacyPolicy.ExecutionPolicy)
	}
	if _, ok := legacyPolicy.Environment["INITIAL_AGENT_MODE"]; ok {
		t.Fatal("the legacy policy set INITIAL_AGENT_MODE before enforcement")
	}
	runtimeRelayMust(t, enforceExecutionPolicy(&legacyPolicy))
	if got, ok := legacyPolicy.Environment["INITIAL_AGENT_MODE"]; !ok || got != "agent-full-access" {
		t.Fatalf("INITIAL_AGENT_MODE = %q", got)
	}

	// A Muse launch an older controller wrote for a profile that opted back
	// into review on unconstrained targets (`muse_auto_review`, 2.60 to 2.71)
	// runs without permission checks: the policy owns the variable.
	muse := runtimeRelayCloneLaunch(launch)
	muse.Harness = core.HarnessKindMuse
	muse.ExecutionPolicy = core.ExecutionPolicyUnconstrained
	muse.Environment["MUSE_APPROVAL_MODE"] = "promptUnmatched"
	runtimeRelayMust(t, enforceExecutionPolicy(&muse))
	if got, ok := muse.Environment["MUSE_APPROVAL_MODE"]; !ok || got != "allowAll" {
		t.Fatalf("MUSE_APPROVAL_MODE = %q", got)
	}

	legacy := runtimeRelayJSONObject(t, launch)
	for _, field := range []string{"additional_directories", "native_session_id"} {
		delete(legacy, field)
	}
	parsed, err := runtimeRelayFromJSONObject[core.WorkerLaunchConfig](t, legacy)
	runtimeRelayMust(t, err)
	if len(parsed.AdditionalDirectories) != 0 {
		t.Fatalf("additional directories = %v", parsed.AdditionalDirectories)
	}
	if parsed.NativeSessionID != nil {
		t.Fatalf("native session = %v", *parsed.NativeSessionID)
	}
	if parsed.ExecutionPolicy != core.ExecutionPolicyUnconstrained {
		t.Fatalf("execution policy = %v", parsed.ExecutionPolicy)
	}

	supervisor := AcpSupervisorSpec{
		Command:             "codex-acp",
		Args:                []string{},
		Environment:         map[string]string{},
		ExcludedEnvironment: []string{},
		Cwd:                 ".",
		HarnessLease:        nil,
		NodeLease:           nil,
	}
	incompleteSpec := runtimeRelayJSONObject(t, supervisor)
	delete(incompleteSpec, "environment")
	if _, err := runtimeRelayFromJSONObject[AcpSupervisorSpec](t, incompleteSpec); err == nil {
		t.Fatal("a supervisor spec without environment was accepted")
	}
	legacySpec := runtimeRelayJSONObject(t, supervisor)
	delete(legacySpec, "node_lease")
	spec, err := runtimeRelayFromJSONObject[AcpSupervisorSpec](t, legacySpec)
	runtimeRelayMust(t, err)
	if spec.NodeLease != nil {
		t.Fatalf("node lease = %q", *spec.NodeLease)
	}
}

// runtimeRelayTestsPrompt is a prompt command of one text block (Rust's
// prompt).
func runtimeRelayTestsPrompt(text string) core.RelayCommand {
	return core.RelayCommandPrompt{Prompt: []acpschema.ContentBlock{runtimeRelayTextBlock(text)}}
}

// runtimeRelayTestsSubmit submits command under commandID and requires the
// relay to accept it (Rust's submit).
func runtimeRelayTestsSubmit(t *testing.T, relay *DurableRelay, commandID string, command core.RelayCommand) {
	t.Helper()
	response := relay.Handle(core.RelayRequestEnvelope{
		RequestID:       "submit-" + commandID,
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         core.RelayRequestSubmit{CommandID: commandID, Command: command},
	})
	if body, ok := response.Body.(core.RelayResponseBodyOk); ok {
		if _, ok := body.Payload.(core.RelayResponsePayloadAccepted); ok {
			return
		}
	}
	t.Fatalf("relay command was not accepted: %#v", response)
}

// runtimeRelaySubmitLocked submits through the cell's lock.
func runtimeRelaySubmitLocked(t *testing.T, relay *RelayCell, commandID string, command core.RelayCommand) {
	t.Helper()
	locked := relay.Lock()
	defer locked.Unlock()
	runtimeRelayTestsSubmit(t, locked.DurableRelay, commandID, command)
}

// elicitationRequest is an out-of-band ACP command: it shares the dispatch
// channel but never reaches durable relay state.
func elicitationRequest() CommandRequest {
	resolved := make(chan error, 1)
	return CommandRequestResolveElicitation{
		ElicitationID: "out-of-band",
		Response:      core.ElicitationResponse{Action: core.ElicitationResponseDecline},
		Resolved:      resolved,
	}
}

// recordedWarning is whether a runtime warning reached durable state. A
// coordinator that parked on a command send stops draining runtime events,
// so this stays false forever once that happens.
func recordedWarning(relay *RelayCell, message string) bool {
	locked := relay.Lock()
	defer locked.Unlock()
	events, err := locked.EventsAfter(0, core.RelayEventGenesisDigest)
	if err != nil {
		panic(err)
	}
	for _, event := range events {
		if warning, ok := event.Observation.(core.RelayObservationWarning); ok && warning.Message == message {
			return true
		}
	}
	return false
}

func nextCommand(t *testing.T, commands <-chan CommandRequest) CommandRequest {
	t.Helper()
	timer := time.NewTimer(5 * time.Second)
	defer timer.Stop()
	var command CommandRequest
	var open bool
	select {
	case command, open = <-commands:
		if !open {
			t.Fatal("the ACP command channel closed")
		}
	case <-timer.C:
		t.Fatal("the relay coordinator stopped dispatching commands")
	}
	return command
}

// runtimeRelayAuthRequired is agent_client_protocol::Error::auth_required().
func runtimeRelayAuthRequired() *acp.RPCError {
	return &acp.RPCError{Code: acp.ErrorCodeAuthRequired, Message: "Authentication required"}
}

// runtimeRelayOperationalState reads the relay's operational state under
// its lock.
func runtimeRelayOperationalState(relay *RelayCell) core.RelayOperationalState {
	locked := relay.Lock()
	defer locked.Unlock()
	return locked.OperationalState()
}

// runtimeRelayIs is whether value is set and equal to want (Rust:
// as_deref() == Some(want)).
func runtimeRelayIs(value *string, want string) bool {
	return value != nil && *value == want
}

// Hard-won: #1515: credential installs did not resume an auth-failed Claude turn.
func TestAuthenticationRecoveryHandlesBothOrdersOfInstallAndFailure(t *testing.T) {
	for _, installFirst := range []bool{true, false} {
		temp := t.TempDir()
		root := filepath.Join(temp, "relay")
		home := filepath.Join(temp, "claude")
		runtimeRelayMkdirAll(t, home)
		path := filepath.Join(home, ".credentials.json")
		old := []byte(`{"claudeAiOauth":{"accessToken":"old-access","refreshToken":"remove-me","expiresAt":1000}}`)
		stripped := []byte(`{"claudeAiOauth":{"accessToken":"old-access","expiresAt":1000}}`)
		fresh := []byte(`{"claudeAiOauth":{"accessToken":"new-access","expiresAt":2000}}`)
		runtimeRelayWriteFile(t, path, old)
		durable := runtimeRelayOpen(t, root, "test")
		setVerdictHarness(t, durable, core.HarnessKindClaude)
		loginPath := path
		durable.authenticationLoginPath = &loginPath
		initialRevision, _ := durable.loginIdentity.snapshot()
		relay := NewRelayCell(durable)
		events := make(chan core.RuntimeEvent, 16)
		wakes := newDispatchWakeChannel()
		commands := newCommandChannel(8)
		coordinator, stopCoordinator := runtimeRelaySpawnCoordinator(relay, events, wakes, commands)
		events <- core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}}

		client, server := runtimeRelayPair(t)
		fatal, _ := fatalReports()
		serving := core.Spawn(func() error {
			return serveClient(context.Background(), server, relay, wakes, credentialEndpointResult{endpoint: CredentialEndpoint{
				Harness:             core.HarnessKindClaude,
				Home:                home,
				Marker:              path,
				NoGithubCredentials: false,
			}}, commands, fatal)
		})
		reader := bufio.NewReader(client)
		delegationExchange(t, reader, client, core.RelayRequestSubmit{
			CommandID: "user-request",
			Command:   runtimeRelayTestsPrompt("finish this task"),
		})
		assertPrompt(t, nextCommand(t, commands.receiver()), "user-request", "finish this task")
		diagnostic := core.TurnDiagnosticFromACP(runtimeRelayAuthRequired())
		fingerprint := core.CredentialFingerprint([]byte("old-access"))
		diagnostic.LoginFingerprint = &fingerprint
		failed := core.RuntimeEventPromptFinished{
			RequestID:  "user-request",
			StopReason: "error",
			Diagnostic: &diagnostic,
			Usage:      nil,
		}
		if !installFirst {
			events <- failed
			waitUntil(t, func() bool {
				return runtimeRelayIs(runtimeRelayOperationalState(relay).Continuation.CompletedCommandID, "user-request")
			}, "failure was not recorded")
		}
		// A document change without an access-token change is not recovery.
		delegationExchange(t, reader, client, core.RelayRequestInstallCredentials{
			Data: base64.StdEncoding.EncodeToString(stripped),
		})
		events <- core.RuntimeEventWarning{Message: "unchanged-login-checked"}
		waitUntil(t, func() bool { return recordedWarning(relay, "unchanged-login-checked") }, "credential check was not drained")
		runtimeRelayNoCommand(t, commands)
		delegationExchange(t, reader, client, core.RelayRequestInstallCredentials{
			Data: base64.StdEncoding.EncodeToString(fresh),
		})
		if installFirst {
			// A startup sample that predates the push cannot overwrite the
			// identity published with the successful install.
			locked := relay.Lock()
			locked.loginIdentity.observe(initialRevision, &fingerprint)
			locked.Unlock()
			events <- failed
		}
		retry, ok := nextCommand(t, commands.receiver()).(CommandRequestPrompt)
		if !ok {
			t.Fatal("recovery must dispatch a prompt")
		}
		if !strings.HasPrefix(retry.RequestID, "auth-retry-") {
			t.Fatalf("retry request = %q", retry.RequestID)
		}
		if !reflect.DeepEqual(retry.Prompt, core.PromptBlocks()) {
			t.Fatalf("retry prompt = %#v", retry.Prompt)
		}
		if got := runtimeRelayOperationalState(relay).Continuation.UserCommandID; !runtimeRelayIs(got, "user-request") {
			t.Fatalf("user command = %v", got)
		}
		// Repeated delivery cannot queue another recovery while the first runs.
		delegationExchange(t, reader, client, core.RelayRequestInstallCredentials{
			Data: base64.StdEncoding.EncodeToString(fresh),
		})
		events <- core.RuntimeEventWarning{Message: "duplicate-login-checked"}
		waitUntil(t, func() bool { return recordedWarning(relay, "duplicate-login-checked") }, "duplicate credential check was not drained")
		runtimeRelayNoCommand(t, commands)
		events <- core.RuntimeEventPromptFinished{
			RequestID:  retry.RequestID,
			StopReason: "EndTurn",
			Diagnostic: nil,
			Usage:      nil,
		}
		events <- core.RuntimeEventStopped{}
		runtimeRelayJoinOk(t, coordinator, "relay coordinator")
		stopCoordinator()
		runtimeRelayMust(t, client.CloseWrite())
		runtimeRelayJoinOk(t, serving, "serve the credential connection")
	}
}

// runtimeRelayNoCommand requires that no ACP command is waiting (Rust:
// command_rx.try_recv().is_err()).
func runtimeRelayNoCommand(t *testing.T, commands *commandChannel) {
	t.Helper()
	select {
	case command := <-commands.receiver():
		t.Fatalf("unexpected ACP command %#v", command)
	default:
	}
}

// Hard-won: #1515: awaiting credential I/O parked the coordinator's event loop.
func TestAStalledLoginReadDoesNotBlockRelayEventsOrShutdown(t *testing.T) {
	root := t.TempDir()
	path := filepath.Join(root, "login.json")
	gate := stalledReadFile(path)
	durable := runtimeRelayOpen(t, root, "test")
	setVerdictHarness(t, durable, core.HarnessKindClaude)
	durable.authenticationLoginPath = &path
	_, err := durable.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	runtimeRelayMust(t, err)
	runtimeRelayTestsSubmit(t, durable, "user-request", runtimeRelayTestsPrompt("finish this task"))
	_, err = durable.ClaimPendingCommands(true)
	runtimeRelayMust(t, err)
	diagnostic := core.TurnDiagnosticFromACP(runtimeRelayAuthRequired())
	diagnostic.LoginFingerprint = runtimeRelayString("old-login")
	_, err = durable.RecordCommandCompleted("user-request", core.RelayCommandOutcomePrompt{
		WorkFacts:  nil,
		StopReason: "error",
		Diagnostic: &diagnostic,
		Usage:      nil,
	})
	runtimeRelayMust(t, err)
	relay := NewRelayCell(durable)
	events := make(chan core.RuntimeEvent, 4)
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator, stopCoordinator := runtimeRelaySpawnCoordinator(relay, events, wakes, commands)
	defer stopCoordinator()
	events <- core.RuntimeEventWarning{Message: "read-is-stalled"}
	waitUntil(t, func() bool { return recordedWarning(relay, "read-is-stalled") }, "login read blocked runtime events")
	events <- core.RuntimeEventStopped{}
	if err := testSupportWaitFor("coordinator shutdown during a login read", func() error {
		value, joinErr := coordinator.Join(context.Background())
		if joinErr != nil {
			return joinErr
		}
		return value
	}); err != nil {
		t.Fatal(err)
	}
	gate.Close()
}

// waitUntil waits for a coordinator-side condition. Everything this guards
// against wedges permanently, so a generous deadline still fails fast
// enough.
func waitUntil(t *testing.T, condition func() bool, blocked string) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for !condition() {
		if !time.Now().Before(deadline) {
			t.Fatal(blocked)
		}
		time.Sleep(time.Millisecond)
	}
}

func assertPrompt(t *testing.T, command CommandRequest, expectedID, expectedText string) {
	t.Helper()
	prompt, ok := command.(CommandRequestPrompt)
	if !ok {
		t.Fatalf("expected ACP prompt command, got %#v", command)
	}
	if prompt.RequestID != expectedID {
		t.Fatalf("request ID = %q, want %q", prompt.RequestID, expectedID)
	}
	if len(prompt.Prompt) != 1 || prompt.Prompt[0].Kind != acpschema.ContentBlockKindText ||
		prompt.Prompt[0].Text == nil || prompt.Prompt[0].Text.Text != expectedText {
		t.Fatalf("prompt = %#v, want one text block %q", prompt.Prompt, expectedText)
	}
}

// runtimeRelayKimiSession writes a Kimi home whose session index names one
// session directory for the fixture session, and returns the home, the
// session directory and its main wire.
func runtimeRelayKimiSession(t *testing.T, temp string) (home, session, wire string) {
	t.Helper()
	home = filepath.Join(temp, "kimi")
	session = filepath.Join(home, "sessions/workspace", runtimeRelayTestsSessionID)
	wire = filepath.Join(session, "agents/main/wire.jsonl")
	runtimeRelayMkdirAll(t, filepath.Dir(wire))
	runtimeRelayWriteFile(t, filepath.Join(session, "state.json"), runtimeRelayJSON(t, map[string]any{"id": runtimeRelayTestsSessionID}))
	runtimeRelayWriteFile(t, filepath.Join(home, "session_index.jsonl"), append(runtimeRelayJSON(t, map[string]any{
		"sessionId":  runtimeRelayTestsSessionID,
		"sessionDir": session,
	}), '\n'))
	return home, session, wire
}

// runtimeRelayJSON is serde_json's compact encoding of value.
func runtimeRelayJSON(t *testing.T, value any) []byte {
	t.Helper()
	encoded, err := json.Marshal(value)
	runtimeRelayMust(t, err)
	return encoded
}

// runtimeRelayJSONLines encodes each record as one JSON line.
func runtimeRelayJSONLines(t *testing.T, records ...any) []byte {
	t.Helper()
	var out []byte
	for _, record := range records {
		out = append(out, runtimeRelayJSON(t, record)...)
		out = append(out, '\n')
	}
	return out
}

// runtimeRelayKimiPromptInFlight opens a Kimi relay in root with a
// configured session and the prompt commandID dispatched, returning it and
// the prompt's start.
func runtimeRelayKimiPromptInFlight(t *testing.T, root, commandID, text string) (*DurableRelay, int64) {
	t.Helper()
	durable := runtimeRelayOpen(t, root, "1.0.0")
	durable.SetBackgroundWorkPolicy(BackgroundWorkPolicyKimiTasks)
	_, err := durable.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	runtimeRelayMust(t, err)
	runtimeRelayTestsSubmit(t, durable, commandID, runtimeRelayTestsPrompt(text))
	_, err = durable.ClaimPendingCommands(true)
	runtimeRelayMust(t, err)
	active := durable.OperationalState().ActivePrompt
	if active == nil {
		t.Fatal("the prompt is not active")
	}
	return durable, active.StartedAtMs
}

// runtimeRelayEvents is every journaled event of relay.
func runtimeRelayEvents(t *testing.T, relay *DurableRelay) []core.RelayEvent {
	t.Helper()
	events, err := relay.EventsAfter(0, core.RelayEventGenesisDigest)
	runtimeRelayMust(t, err)
	return events
}

// runtimeRelayLockedEvents is every journaled event, read under the lock.
func runtimeRelayLockedEvents(t *testing.T, relay *RelayCell) []core.RelayEvent {
	t.Helper()
	locked := relay.Lock()
	defer locked.Unlock()
	return runtimeRelayEvents(t, locked.DurableRelay)
}

// runtimeRelayPromptOutcome is the prompt outcome of a CommandCompleted
// event, if it is one.
func runtimeRelayPromptOutcome(event core.RelayEvent) (core.RelayCommandOutcomePrompt, bool) {
	completed, ok := event.Observation.(core.RelayObservationCommandCompleted)
	if !ok {
		return core.RelayCommandOutcomePrompt{}, false
	}
	outcome, ok := completed.Outcome.(core.RelayCommandOutcomePrompt)
	return outcome, ok
}

func TestKimiDiagnosticIsEnrichedBeforeDurableCompletion(t *testing.T) {
	temp := t.TempDir()
	home, _, wire := runtimeRelayKimiSession(t, temp)
	runtimeRelayWriteFile(t, wire, nil)
	root := filepath.Join(temp, "relay")
	durable, started := runtimeRelayKimiPromptInFlight(t, root, "quota-prompt", "test")
	relay := NewRelayCell(durable)
	monitor := newKimiTaskMonitor(kimiHome{path: home})
	ctx := context.Background()
	runtimeRelayMust(t, monitor.attach(ctx, runtimeRelayTestsSessionID, relay))
	// Large intervening output exercises the existing bounded native follower.
	runtimeRelayWriteFile(t, wire, runtimeRelayJSONLines(t,
		map[string]any{"type": "turn.started", "agentId": "main", "turnId": 1, "time": started},
		map[string]any{"type": "assistant.delta", "text": strings.Repeat("x", 80*1024)},
		map[string]any{"type": "turn.ended", "agentId": "main", "turnId": 1, "reason": "failed", "error": map[string]any{"code": "provider.auth_error", "message": "Five-hour usage limit exceeded", "details": map[string]any{"statusCode": 403}}},
	))
	var event core.RuntimeEvent = core.RuntimeEventPromptFinished{
		RequestID:  "quota-prompt",
		StopReason: "error",
		Usage:      nil,
		Diagnostic: nil,
	}
	runtimeRelayMust(t, prepareKimiRuntimeEvent(ctx, monitor, relay, &event))
	_, err := recordRuntimeEvent(relay, map[string]core.RelayCommand{}, event)
	runtimeRelayMust(t, err)
	{
		locked := relay.Lock()
		if _, ok := locked.CapacityRetryDeadline(); ok {
			t.Fatal("a quota failure scheduled a capacity retry")
		}
		locked.Unlock()
	}
	reopened := runtimeRelayOpen(t, root, "1.0.0")
	found := false
	for _, event := range runtimeRelayEvents(t, reopened) {
		if outcome, ok := runtimeRelayPromptOutcome(event); ok && outcome.StopReason == "QuotaLimit" &&
			outcome.Diagnostic != nil && outcome.Diagnostic.HTTPStatus != nil && *outcome.Diagnostic.HTTPStatus == 403 {
			found = true
		}
	}
	if !found {
		t.Fatal("the completed prompt does not carry the enriched quota diagnostic")
	}
}

// #1132: Kimi refused its stored OAuth token, journalled the failure, and
// then completed the ACP prompt with no session updates at all. Mjolnir
// reported `prompt_unanswered` and suggested resending, which only repeats
// the failure. When Kimi journalled a failure for the turn, that failure is
// the turn's outcome: its warning and its diagnostic are the authentication
// error, and credential sync treats it as one.
// Hard-won: #1132: Kimi's journalled OAuth failure was misreported as unanswered and suggested an unsafe retry.
func TestAnEmptyKimiTurnWithAJournalledLoginFailureReportsTheFailure(t *testing.T) {
	temp := t.TempDir()
	home, _, wire := runtimeRelayKimiSession(t, temp)
	runtimeRelayWriteFile(t, wire, nil)
	root := filepath.Join(temp, "relay")
	durable, started := runtimeRelayKimiPromptInFlight(t, root, "continue", "continue")
	relay := NewRelayCell(durable)
	monitor := newKimiTaskMonitor(kimiHome{path: home})
	ctx := context.Background()
	runtimeRelayMust(t, monitor.attach(ctx, runtimeRelayTestsSessionID, relay))
	// What Kimi Code 2.0.2 journals for the turn, as in the issue.
	runtimeRelayWriteFile(t, wire, runtimeRelayJSONLines(t,
		map[string]any{"type": "agent.turn.started", "turnId": 3, "queueItemId": "q", "time": started, "kind": "event"},
		map[string]any{"type": "turn.prompt", "agentId": "main", "input": []any{}, "origin": map[string]any{"kind": "user"}, "promptId": "msg_3", "turnId": 3, "time": started},
		map[string]any{"type": "agent.turn.ended", "turnId": 3, "outcome": "failed", "errorMessage": "OAuthUnauthorizedError: Stored token for \"kimi-code\" was rejected; re-login required.", "time": started + 2, "kind": "event"},
		map[string]any{"type": "turn.ended", "agentId": "main", "turnId": 3, "reason": "failed", "error": map[string]any{"code": "internal", "message": "Stored token for \"kimi-code\" was rejected; re-login required.", "name": "OAuthUnauthorizedError", "retryable": false}, "durationMs": 3, "time": started + 3},
		map[string]any{"type": "prompt.completed", "agentId": "main", "promptId": "msg_3", "reason": "failed", "time": started + 4},
	))

	// What the ACP runtime emits for a prompt that completed with no updates.
	unanswered := fmt.Sprintf("%s: Kimi Code ended the turn without producing any message, thought or tool call, so this prompt may never have been acted on.", core.PromptEmptyResponseMarker)
	code := core.PromptUnansweredStopReason
	runtimeEvents := []core.RuntimeEvent{
		core.RuntimeEventWarning{Message: unanswered},
		core.RuntimeEventPromptFinished{
			RequestID:  "continue",
			StopReason: core.PromptUnansweredStopReason,
			Usage:      nil,
			Diagnostic: &core.TurnDiagnostic{
				Message:          unanswered,
				Code:             &code,
				HTTPStatus:       nil,
				ResetAt:          nil,
				LoginFingerprint: nil,
			},
		},
	}
	for _, event := range runtimeEvents {
		runtimeRelayMust(t, prepareKimiRuntimeEvent(ctx, monitor, relay, &event))
		_, err := recordRuntimeEvent(relay, map[string]core.RelayCommand{}, event)
		runtimeRelayMust(t, err)
	}

	reopened := runtimeRelayOpen(t, root, "1.0.0")
	events := runtimeRelayEvents(t, reopened)
	warnings := []string{}
	for _, event := range events {
		if warning, ok := event.Observation.(core.RelayObservationWarning); ok {
			warnings = append(warnings, warning.Message)
		}
	}
	if want := []string{"prompt failed: Stored token for \"kimi-code\" was rejected; re-login required."}; !slices.Equal(warnings, want) {
		t.Fatalf("warnings = %q, want %q", warnings, want)
	}
	index := slices.IndexFunc(events, func(event core.RelayEvent) bool {
		_, ok := event.Observation.(core.RelayObservationCommandCompleted)
		return ok
	})
	if index < 0 {
		t.Fatal("the prompt completed")
	}
	completed := events[index]
	outcome, ok := runtimeRelayPromptOutcome(completed)
	if !ok || outcome.Diagnostic == nil {
		t.Fatalf("%#v", completed)
	}
	if outcome.StopReason != "error" {
		t.Fatalf("stop reason = %q", outcome.StopReason)
	}
	if outcome.Diagnostic.Message != "Stored token for \"kimi-code\" was rejected; re-login required." {
		t.Fatalf("diagnostic = %q", outcome.Diagnostic.Message)
	}
	if reason, ok := core.RelayEventCredentialSyncReason(&completed); !ok || reason != core.CredentialSyncReasonAuthenticationFailure {
		t.Fatalf("credential sync reason = %v, %v", reason, ok)
	}
}

// Kimi's ACP prompt response carries no usage, so a completed Kimi turn is
// only accounted for if the native wire records reach the durable relay
// (#1064).
// Hard-won: #1064: Kimi ACP omitted usage unless the worker imported native turn records.
func TestKimiTurnUsageIsRecordedFromNativeRecords(t *testing.T) {
	temp := t.TempDir()
	home, _, wire := runtimeRelayKimiSession(t, temp)
	runtimeRelayWriteFile(t, wire, nil)
	root := filepath.Join(temp, "relay")
	durable, started := runtimeRelayKimiPromptInFlight(t, root, "usage-prompt", "test")
	relay := NewRelayCell(durable)
	monitor := newKimiTaskMonitor(kimiHome{path: home})
	ctx := context.Background()
	runtimeRelayMust(t, monitor.attach(ctx, runtimeRelayTestsSessionID, relay))
	runtimeRelayWriteFile(t, wire, runtimeRelayJSONLines(t,
		map[string]any{"type": "turn.started", "agentId": "main", "turnId": 1, "time": started},
		map[string]any{"type": "usage.record", "model": "kimi-code/k3", "usage": map[string]any{"inputOther": 12504, "output": 416, "inputCacheRead": 11264, "inputCacheCreation": 0}, "usageScope": "turn"},
		map[string]any{"type": "usage.record", "model": "kimi-code/k3", "usage": map[string]any{"inputOther": 717, "output": 25, "inputCacheRead": 23552, "inputCacheCreation": 0}, "usageScope": "turn"},
		map[string]any{"type": "turn.ended", "agentId": "main", "turnId": 1, "reason": "completed"},
	))
	var event core.RuntimeEvent = core.RuntimeEventPromptFinished{
		RequestID:  "usage-prompt",
		StopReason: "EndTurn",
		Usage:      nil,
		Diagnostic: nil,
	}
	runtimeRelayMust(t, prepareKimiRuntimeEvent(ctx, monitor, relay, &event))
	_, err := recordRuntimeEvent(relay, map[string]core.RelayCommand{}, event)
	runtimeRelayMust(t, err)
	reopened := runtimeRelayOpen(t, root, "1.0.0")
	var usage *core.TokenUsage
	for _, event := range runtimeRelayEvents(t, reopened) {
		if outcome, ok := runtimeRelayPromptOutcome(event); ok && outcome.Usage != nil {
			usage = outcome.Usage
			break
		}
	}
	if usage == nil {
		t.Fatal("the completed Kimi prompt recorded no usage")
	}
	if usage.Scope != core.UsageScopeTurn {
		t.Fatalf("scope = %v", usage.Scope)
	}
	if usage.InputTokens != 12504+717+11264+23552 {
		t.Fatalf("input tokens = %d", usage.InputTokens)
	}
	if usage.OutputTokens != 416+25 {
		t.Fatalf("output tokens = %d", usage.OutputTokens)
	}
	if usage.CachedReadTokens == nil || *usage.CachedReadTokens != 11264+23552 {
		t.Fatalf("cached read tokens = %v", usage.CachedReadTokens)
	}
}

// runtimeRelayKimiConfigured opens a Kimi relay with a configured session.
func runtimeRelayKimiConfigured(t *testing.T, root string) *RelayCell {
	t.Helper()
	durable := runtimeRelayOpen(t, root, "1.0.0")
	durable.SetBackgroundWorkPolicy(BackgroundWorkPolicyKimiTasks)
	_, err := durable.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	runtimeRelayMust(t, err)
	return NewRelayCell(durable)
}

func runtimeRelayRecordSessionUpdate(t *testing.T, relay *RelayCell, update acpschema.SessionUpdate) {
	t.Helper()
	locked := relay.Lock()
	defer locked.Unlock()
	_, err := locked.RecordSessionUpdate(update)
	runtimeRelayMust(t, err)
}

func TestKimiNativeTaskLevelBlocksReplacementUntilTermination(t *testing.T) {
	temp := t.TempDir()
	home, _, wire := runtimeRelayKimiSession(t, temp)
	started := map[string]any{
		"type":    "task.started",
		"agentId": "main",
		"info": map[string]any{
			"taskId":           "agent-1",
			"description":      "fix the regression",
			"status":           "running",
			"detached":         true,
			"startedAt":        1234,
			"kind":             "agent",
			"agentId":          "agent-1",
			"parentToolCallId": "tool-agent",
		},
	}
	runtimeRelayWriteFile(t, wire, runtimeRelayJSONLines(t, started))

	relay := runtimeRelayKimiConfigured(t, filepath.Join(temp, "relay"))
	monitor := newKimiTaskMonitor(kimiHome{path: home})
	ctx := context.Background()
	runtimeRelayMust(t, monitor.attach(ctx, runtimeRelayTestsSessionID, relay))

	taskQuery := func() acpschema.SessionUpdate {
		status := acpschema.ToolCallStatusCompleted
		return acpschema.SessionUpdate{Kind: acpschema.SessionUpdateKindToolCall, ToolCall: &acpschema.ToolCall{
			ToolCallID: "0:query-not-launcher",
			Title:      "TaskOutput",
			Status:     &status,
			RawOutput:  runtimeRelayJSON(t, map[string]any{"output": "task_id: agent-1\nstatus: running"}),
		}}
	}
	runtimeRelayRecordSessionUpdate(t, relay, taskQuery())

	running := runtimeRelayOperationalState(relay)
	if len(running.BackgroundCommands) != 0 {
		t.Fatalf("background commands = %#v", running.BackgroundCommands)
	}
	if running.NativeAgentCount != 1 {
		t.Fatalf("native agents = %d", running.NativeAgentCount)
	}
	if running.SafeToReplace(core.HarnessKindKimi) {
		t.Fatal("a running native task allowed replacement")
	}

	terminated := map[string]any{
		"type":    "task.terminated",
		"agentId": "main",
		"info": map[string]any{
			"taskId":           "agent-1",
			"status":           "completed",
			"agentId":          "agent-1",
			"parentToolCallId": "tool-agent",
		},
	}
	file, err := os.OpenFile(wire, os.O_APPEND|os.O_WRONLY, 0)
	runtimeRelayMust(t, err)
	_, err = file.Write(runtimeRelayJSONLines(t, terminated))
	runtimeRelayMust(t, err)
	runtimeRelayMust(t, file.Close())
	runtimeRelayMust(t, monitor.refresh(ctx, relay, true))

	// A delayed query response must not resurrect a terminated native task.
	runtimeRelayRecordSessionUpdate(t, relay, taskQuery())
	finished := runtimeRelayOperationalState(relay)
	if len(finished.BackgroundCommands) != 0 {
		t.Fatalf("background commands = %#v", finished.BackgroundCommands)
	}
	if finished.NativeAgentCount != 0 {
		t.Fatalf("native agents = %d", finished.NativeAgentCount)
	}
	if !finished.SafeToReplace(core.HarnessKindKimi) {
		t.Fatal("a terminated native task still blocks replacement")
	}
}

func TestKimiNativeTaskMonitorFollowsNewestSessionIndexWire(t *testing.T) {
	temp := t.TempDir()
	home := filepath.Join(temp, "kimi")
	oldSession := filepath.Join(home, "sessions/old", runtimeRelayTestsSessionID)
	newSession := filepath.Join(home, "sessions/new", runtimeRelayTestsSessionID)
	for _, session := range []struct {
		directory string
		taskIDs   []string
	}{
		{oldSession, []string{"old-agent"}},
		{newSession, []string{"new-agent", "second-agent"}},
	} {
		wire := filepath.Join(session.directory, "agents/main/wire.jsonl")
		runtimeRelayMkdirAll(t, filepath.Dir(wire))
		runtimeRelayWriteFile(t, filepath.Join(session.directory, "state.json"), runtimeRelayJSON(t, map[string]any{"id": runtimeRelayTestsSessionID}))
		records := []string{}
		for _, taskID := range session.taskIDs {
			records = append(records, string(runtimeRelayJSON(t, map[string]any{
				"type":    "task.started",
				"agentId": "main",
				"info": map[string]any{
					"taskId":      taskID,
					"description": taskID,
					"status":      "running",
					"detached":    true,
					"startedAt":   1234,
					"kind":        "agent",
					"agentId":     "agent",
				},
			})))
		}
		runtimeRelayWriteFile(t, wire, []byte(strings.Join(records, "\n")+"\n"))
	}
	runtimeRelayWriteFile(t, filepath.Join(home, "session_index.jsonl"), runtimeRelayJSONLines(t,
		map[string]any{"sessionId": runtimeRelayTestsSessionID, "sessionDir": oldSession}))

	relay := runtimeRelayKimiConfigured(t, filepath.Join(temp, "relay"))
	monitor := newKimiTaskMonitor(kimiHome{path: home})
	ctx := context.Background()
	runtimeRelayMust(t, monitor.attach(ctx, runtimeRelayTestsSessionID, relay))
	if got := runtimeRelayOperationalState(relay).NativeAgentCount; got != 1 {
		t.Fatalf("native agents = %d", got)
	}

	runtimeRelayWriteFile(t, filepath.Join(home, "session_index.jsonl"), runtimeRelayJSONLines(t,
		map[string]any{"sessionId": runtimeRelayTestsSessionID, "sessionDir": oldSession},
		map[string]any{"sessionId": runtimeRelayTestsSessionID, "sessionDir": newSession},
	))
	runtimeRelayMust(t, monitor.refresh(ctx, relay, true))

	state := runtimeRelayOperationalState(relay)
	if len(state.BackgroundCommands) != 0 {
		t.Fatalf("background commands = %#v", state.BackgroundCommands)
	}
	if state.NativeAgentCount != 2 {
		t.Fatalf("native agents = %d", state.NativeAgentCount)
	}
}

func TestKimiNativeTaskMonitorRetainsTasksAndDeduplicatesFailureWarning(t *testing.T) {
	temp := t.TempDir()
	home, _, wire := runtimeRelayKimiSession(t, temp)
	runtimeRelayWriteFile(t, wire, runtimeRelayJSONLines(t, map[string]any{
		"type":    "task.started",
		"agentId": "main",
		"info": map[string]any{
			"taskId":      "agent-1",
			"description": "still running",
			"status":      "running",
			"detached":    true,
			"startedAt":   1234,
			"kind":        "agent",
			"agentId":     "agent",
		},
	}))

	relay := runtimeRelayKimiConfigured(t, filepath.Join(temp, "relay"))
	monitor := newKimiTaskMonitor(kimiHome{path: home})
	ctx := context.Background()
	runtimeRelayMust(t, monitor.attach(ctx, runtimeRelayTestsSessionID, relay))
	runtimeRelayMust(t, os.Remove(filepath.Join(home, "session_index.jsonl")))

	runtimeRelayMust(t, monitor.refresh(ctx, relay, true))
	afterFailure := runtimeRelayOperationalState(relay)
	if len(afterFailure.BackgroundCommands) != 0 {
		t.Fatalf("background commands = %#v", afterFailure.BackgroundCommands)
	}
	if afterFailure.NativeAgentCount != 1 {
		t.Fatalf("native agents = %d", afterFailure.NativeAgentCount)
	}
	if afterFailure.BackgroundWorkKnown == nil || *afterFailure.BackgroundWorkKnown {
		t.Fatalf("background work known = %v", afterFailure.BackgroundWorkKnown)
	}
	if afterFailure.SafeToReplace(core.HarnessKindKimi) {
		t.Fatal("unknown background work allowed replacement")
	}

	runtimeRelayMust(t, monitor.refresh(ctx, relay, true))
	warningCount := 0
	for _, event := range runtimeRelayLockedEvents(t, relay) {
		if warning, ok := event.Observation.(core.RelayObservationWarning); ok &&
			strings.HasPrefix(warning.Message, "Kimi background task tracking is unavailable;") {
			warningCount++
		}
	}
	if warningCount != 1 {
		t.Fatalf("warnings = %d, want 1", warningCount)
	}
}

// runtimeRelayEmit sends a runtime event and requires room for it (Rust:
// event_tx.send(..).unwrap()).
func runtimeRelayEmit(t *testing.T, events TestRuntimeEventSender, event core.RuntimeEvent) {
	t.Helper()
	if err := events.send(event); err != nil {
		t.Fatal(err)
	}
}

// runtimeRelayCommandWithin receives the next ACP command within limit
// (Rust: timeout(limit, command_rx.recv()).await.unwrap().unwrap()).
func runtimeRelayCommandWithin(t *testing.T, commands *commandChannel, limit time.Duration) CommandRequest {
	t.Helper()
	timer := time.NewTimer(limit)
	defer timer.Stop()
	select {
	case command, open := <-commands.receiver():
		if !open {
			t.Fatal("the ACP command channel closed")
		}
		return command
	case <-timer.C:
		t.Fatalf("no ACP command within %v", limit)
		return nil
	}
}

// runtimeRelayNoCommandWithin requires that no ACP command arrives within
// limit (Rust: timeout(limit, command_rx.recv()).await.is_err()).
func runtimeRelayNoCommandWithin(t *testing.T, commands *commandChannel, limit time.Duration, message string) {
	t.Helper()
	timer := time.NewTimer(limit)
	defer timer.Stop()
	select {
	case command, open := <-commands.receiver():
		if !open {
			t.Fatalf("%s: the ACP command channel closed", message)
		}
		t.Fatalf("%s: %#v", message, command)
	case <-timer.C:
	}
}

// runtimeRelayQueuedCommands is how many commands wait in the channel for
// the receiver (Rust: Receiver::len).
func runtimeRelayQueuedCommands(commands *commandChannel) int {
	return len(commands.out)
}

func runtimeRelayCancelID(command CommandRequest) (string, bool) {
	cancel, ok := command.(CommandRequestCancel)
	return cancel.RequestID, ok
}

func runtimeRelaySetConfigID(command CommandRequest) (string, bool) {
	config, ok := command.(CommandRequestSetConfig)
	return config.RequestID, ok
}

// runtimeRelayCommandCompleted is whether events record commandID
// completing with an outcome of type O.
func runtimeRelayCommandCompleted[O core.RelayCommandOutcome](events []core.RelayEvent, commandID string) bool {
	for _, event := range events {
		completed, ok := event.Observation.(core.RelayObservationCommandCompleted)
		if !ok || completed.CommandID != commandID {
			continue
		}
		if _, ok := completed.Outcome.(O); ok {
			return true
		}
	}
	return false
}

func TestOfflinePromptQueueRunsSeriallyWithoutAController(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "prompt-1", runtimeRelayTestsPrompt("first"))
	runtimeRelayTestsSubmit(t, durable, "prompt-2", runtimeRelayTestsPrompt("second"))
	relay := NewRelayCell(durable)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})

	assertPrompt(t, runtimeRelayCommandWithin(t, commands, time.Second), "prompt-1", "first")
	runtimeRelayEmit(t, events, core.RuntimeEventPromptFinished{
		Diagnostic: nil,
		RequestID:  "prompt-1",
		StopReason: "end_turn",
		Usage:      nil,
	})
	assertPrompt(t, runtimeRelayCommandWithin(t, commands, time.Second), "prompt-2", "second")
	if active := runtimeRelayOperationalState(relay).ActivePrompt; active == nil || active.CommandID != "prompt-2" {
		t.Fatalf("active prompt = %#v", active)
	}

	runtimeRelayEmit(t, events, core.RuntimeEventCommandRejected{
		RequestID: "prompt-2",
		Message:   "test shutdown",
		Reason:    core.OutcomeReasonAdmissionRejected,
	})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

func TestConfigDuringAPromptWaitsButCancelDispatchesImmediately(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "active-prompt", runtimeRelayTestsPrompt("running"))
	relay := NewRelayCell(durable)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	assertPrompt(t, nextCommand(t, commands.receiver()), "active-prompt", "running")

	runtimeRelaySubmitLocked(t, relay, "config-while-running", core.RelayCommandSetConfig{Key: "model", Value: "later"})
	runtimeRelaySubmitLocked(t, relay, "cancel-while-running", core.RelayCommandCancel{})
	runtimeRelayMust(t, wakes.trySend())
	if id, ok := runtimeRelayCancelID(runtimeRelayCommandWithin(t, commands, time.Second)); !ok || id != "cancel-while-running" {
		t.Fatalf("expected the cancel, got %q", id)
	}

	runtimeRelayEmit(t, events, core.RuntimeEventCancelApplied{RequestID: "cancel-while-running"})
	runtimeRelayNoCommandWithin(t, commands, 25*time.Millisecond, "configuration dispatched before the active prompt finished")

	runtimeRelayEmit(t, events, core.RuntimeEventPromptFinished{
		Diagnostic: nil,
		RequestID:  "active-prompt",
		StopReason: "cancelled",
		Usage:      nil,
	})
	if id, ok := runtimeRelaySetConfigID(runtimeRelayCommandWithin(t, commands, time.Second)); !ok || id != "config-while-running" {
		t.Fatalf("expected the configuration change, got %q", id)
	}
	runtimeRelayEmit(t, events, core.RuntimeEventCommandRejected{
		RequestID: "config-while-running",
		Message:   "test shutdown",
		Reason:    core.OutcomeReasonAdmissionRejected,
	})
	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

func TestCancelTurnInterruptsARunningPromptWithoutSteeringOrCuttingACheckpoint(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "active-prompt", runtimeRelayTestsPrompt("running"))
	relay := NewRelayCell(durable)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	assertPrompt(t, nextCommand(t, commands.receiver()), "active-prompt", "running")

	runtimeRelaySubmitLocked(t, relay, "pending-checkpoint", core.RelayCommandBeginCheckpoint{Reason: nil})
	runtimeRelaySubmitLocked(t, relay, "queued-prompt", runtimeRelayTestsPrompt("leave queued"))
	runtimeRelaySubmitLocked(t, relay, "cancel-turn", core.RelayCommandCancelTurn{})
	runtimeRelayMust(t, wakes.trySend())

	cancel, ok := nextCommand(t, commands.receiver()).(CommandRequestCancel)
	if !ok || cancel.RequestID != "cancel-turn" || cancel.SteeringPrompt != nil {
		t.Fatalf("expected a plain cancel, got %#v", cancel)
	}
	runtimeRelayEmit(t, events, core.RuntimeEventCancelApplied{RequestID: "cancel-turn"})
	queuedPromptPresent := func(state core.RelayOperationalState) bool {
		return slices.ContainsFunc(state.QueuedPrompts, func(queued core.QueuedRelayPrompt) bool {
			return queued.CommandID == "queued-prompt"
		})
	}
	waitUntil(t, func() bool {
		locked := relay.Lock()
		defer locked.Unlock()
		state := locked.OperationalState()
		events, err := locked.EventsAfter(0, core.RelayEventGenesisDigest)
		if err != nil {
			panic(err)
		}
		cancelCompleted := runtimeRelayCommandCompleted[core.RelayCommandOutcomeCancelled](events, "cancel-turn")
		return cancelCompleted && state.ActivePrompt != nil && queuedPromptPresent(state) && state.CheckpointBarrier == nil
	}, "CancelApplied did not complete without admitting the pending checkpoint")

	runtimeRelayEmit(t, events, core.RuntimeEventPromptFinished{
		Diagnostic: nil,
		RequestID:  "active-prompt",
		StopReason: "cancelled",
		Usage:      nil,
	})
	waitForRelayState(t, relay, func(state core.RelayOperationalState) bool {
		return state.ActivePrompt == nil && state.CheckpointReady != nil
	})
	if !queuedPromptPresent(runtimeRelayOperationalState(relay)) {
		t.Fatal("the queued prompt was dropped")
	}

	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

// runtimeRelayDuplexBuffer is the capacity of each direction of a test
// duplex, as the Rust fixtures' tokio::io::duplex(1024).
const runtimeRelayDuplexBuffer = 1024

// runtimeRelayPipe is one direction of a duplex: a bounded in-memory byte
// buffer. It is built on channels so that a goroutine waiting on it is
// durably blocked inside a testing/synctest bubble.
type runtimeRelayPipe struct {
	data           chan byte
	writerClosed   chan struct{}
	readerClosed   chan struct{}
	closeWriteOnce sync.Once
	closeReadOnce  sync.Once
}

func newRuntimeRelayPipe() *runtimeRelayPipe {
	return &runtimeRelayPipe{
		data:         make(chan byte, runtimeRelayDuplexBuffer),
		writerClosed: make(chan struct{}),
		readerClosed: make(chan struct{}),
	}
}

func (p *runtimeRelayPipe) write(data []byte) (int, error) {
	for written, value := range data {
		select {
		case <-p.readerClosed:
			return written, io.ErrClosedPipe
		case <-p.writerClosed:
			return written, io.ErrClosedPipe
		default:
		}
		select {
		case p.data <- value:
		case <-p.readerClosed:
			return written, io.ErrClosedPipe
		case <-p.writerClosed:
			return written, io.ErrClosedPipe
		}
	}
	return len(data), nil
}

// read waits for at least one byte and returns what is buffered, or EOF
// once the writer closed and the buffer is empty.
func (p *runtimeRelayPipe) read(buffer []byte) (int, error) {
	if len(buffer) == 0 {
		return 0, nil
	}
	select {
	case value := <-p.data:
		buffer[0] = value
	default:
		select {
		case value := <-p.data:
			buffer[0] = value
		case <-p.readerClosed:
			return 0, io.ErrClosedPipe
		case <-p.writerClosed:
			select {
			case value := <-p.data:
				buffer[0] = value
			default:
				return 0, io.EOF
			}
		}
	}
	read := 1
	for read < len(buffer) {
		select {
		case value := <-p.data:
			buffer[read] = value
			read++
		default:
			return read, nil
		}
	}
	return read, nil
}

func (p *runtimeRelayPipe) closeWrite() { p.closeWriteOnce.Do(func() { close(p.writerClosed) }) }
func (p *runtimeRelayPipe) closeRead()  { p.closeReadOnce.Do(func() { close(p.readerClosed) }) }

// runtimeRelayDuplexEnd is one end of tokio::io::duplex: what the other end
// writes is read here, and CloseWrite ends the other end's reads.
type runtimeRelayDuplexEnd struct {
	incoming *runtimeRelayPipe
	outgoing *runtimeRelayPipe
}

func (e *runtimeRelayDuplexEnd) Read(p []byte) (int, error)  { return e.incoming.read(p) }
func (e *runtimeRelayDuplexEnd) Write(p []byte) (int, error) { return e.outgoing.write(p) }
func (e *runtimeRelayDuplexEnd) CloseWrite() error           { e.outgoing.closeWrite(); return nil }

// Close is Rust's drop of the end: both directions close.
func (e *runtimeRelayDuplexEnd) Close() error {
	e.outgoing.closeWrite()
	e.incoming.closeRead()
	return nil
}

// runtimeRelayDuplex is tokio::io::duplex: two connected in-memory ends.
func runtimeRelayDuplex() (*runtimeRelayDuplexEnd, *runtimeRelayDuplexEnd) {
	forward := newRuntimeRelayPipe()
	backward := newRuntimeRelayPipe()
	return &runtimeRelayDuplexEnd{incoming: backward, outgoing: forward},
		&runtimeRelayDuplexEnd{incoming: forward, outgoing: backward}
}

func runtimeRelayReadExact(t *testing.T, reader io.Reader, length int) []byte {
	t.Helper()
	buffer := make([]byte, length)
	if _, err := io.ReadFull(reader, buffer); err != nil {
		t.Fatal(err)
	}
	return buffer
}

func runtimeRelayWriteAll(t *testing.T, writer io.Writer, data []byte) {
	t.Helper()
	if _, err := writer.Write(data); err != nil {
		t.Fatal(err)
	}
}

// Tokio's paused clock is testing/synctest's bubble here: time advances
// only while every goroutine of the test waits.
func TestProxyTransportDoesNotExpireAnIdleConnection(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		controller, proxyClient := runtimeRelayDuplex()
		proxyRelay, relayPeer := runtimeRelayDuplex()
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		proxy := core.Spawn(func() error {
			return forwardProxyStreams(ctx, proxyClient, proxyClient, proxyRelay, proxyRelay)
		})

		runtimeRelayWriteAll(t, controller, []byte("request"))
		synctest.Wait()
		select {
		case <-proxy.Done():
			t.Fatalf("the proxy stopped before forwarding: %s", runtimeRelayTaskEnding(proxy))
		default:
		}
		if got := runtimeRelayReadExact(t, relayPeer, 7); string(got) != "request" {
			t.Fatalf("relay read %q", got)
		}
		runtimeRelayWriteAll(t, relayPeer, []byte("response"))
		if got := runtimeRelayReadExact(t, controller, 8); string(got) != "response" {
			t.Fatalf("controller read %q", got)
		}

		time.Sleep(16 * time.Minute)
		synctest.Wait()
		select {
		case <-proxy.Done():
			t.Fatal("idle proxy connection expired")
		default:
		}

		runtimeRelayWriteAll(t, controller, []byte("another"))
		if got := runtimeRelayReadExact(t, relayPeer, 7); string(got) != "another" {
			t.Fatalf("relay read %q", got)
		}
		runtimeRelayWriteAll(t, relayPeer, []byte("answer!!"))
		if got := runtimeRelayReadExact(t, controller, 8); string(got) != "answer!!" {
			t.Fatalf("controller read %q", got)
		}

		relayPeer.Close()
		controller.Close()
		runtimeRelayJoinOk(t, proxy, "proxy")
		proxyClient.Close()
		proxyRelay.Close()
	})
}

func TestProxyTransportExpiresWhileWaitingForItsFirstRequest(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		controller, proxyClient := runtimeRelayDuplex()
		proxyRelay, relayPeer := runtimeRelayDuplex()
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		proxy := core.Spawn(func() error {
			return forwardProxyStreams(ctx, proxyClient, proxyClient, proxyRelay, proxyRelay)
		})

		time.Sleep(proxyInitialInputTimeout)
		value, err := proxy.Join(context.Background())
		if err != nil {
			t.Fatalf("proxy task stopped cleanly: %v", err)
		}
		if value != nil {
			t.Fatalf("pre-handshake proxy timeout is clean shutdown: %v", value)
		}
		controller.Close()
		relayPeer.Close()
		proxyClient.Close()
		proxyRelay.Close()
	})
}

func TestPromptDispatchPreservesTheCompleteAcpContentVector(t *testing.T) {
	temp := t.TempDir()
	content := []acpschema.ContentBlock{runtimeRelayTextBlock("first block"), runtimeRelayTextBlock("second block")}
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "prompt-blocks", core.RelayCommandPrompt{Prompt: slices.Clone(content)})
	relay := NewRelayCell(durable)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(1)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})

	prompt, ok := nextCommand(t, commands.receiver()).(CommandRequestPrompt)
	if !ok {
		t.Fatal("expected ACP prompt command")
	}
	if prompt.RequestID != "prompt-blocks" {
		t.Fatalf("request = %q", prompt.RequestID)
	}
	if !reflect.DeepEqual(prompt.Prompt, content) {
		t.Fatalf("prompt = %#v, want %#v", prompt.Prompt, content)
	}

	runtimeRelayEmit(t, events, core.RuntimeEventCommandRejected{
		RequestID: "prompt-blocks",
		Message:   "test shutdown",
		Reason:    core.OutcomeReasonAdmissionRejected,
	})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

func TestSamePriorityQueueEntriesDispatchInAcceptanceOrder(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "z-accepted-first", core.RelayCommandSetConfig{Key: "model", Value: "first"})
	runtimeRelayTestsSubmit(t, durable, "a-accepted-second", core.RelayCommandSetConfig{Key: "model", Value: "second"})
	relay := NewRelayCell(durable)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(2)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})

	// Queue entries run one at a time, so the second change reaches ACP
	// only after the first is terminal.
	if id, ok := runtimeRelaySetConfigID(nextCommand(t, commands.receiver())); !ok || id != "z-accepted-first" {
		t.Fatalf("first dispatch = %q", id)
	}
	runtimeRelayNoCommandWithin(t, commands, 25*time.Millisecond, "the queued change dispatched while an earlier one was in flight")
	runtimeRelayEmit(t, events, core.RuntimeEventCommandRejected{
		RequestID: "z-accepted-first",
		Message:   "advance the queue",
		Reason:    core.OutcomeReasonAdmissionRejected,
	})
	if id, ok := runtimeRelaySetConfigID(runtimeRelayCommandWithin(t, commands, time.Second)); !ok || id != "a-accepted-second" {
		t.Fatalf("second dispatch = %q", id)
	}

	runtimeRelayEmit(t, events, core.RuntimeEventCommandRejected{
		RequestID: "a-accepted-second",
		Message:   "test shutdown",
		Reason:    core.OutcomeReasonAdmissionRejected,
	})
	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

// The context the relay attached to a claimed prompt goes first, in the
// block the ACP runtime turns into what each harness receives.
func TestAClaimedPromptCarriesItsHiddenContextFirst(t *testing.T) {
	user := runtimeRelayTextBlock("ship it")
	claimed := func(hiddenPromptContext *string) core.ClaimedRelayCommand {
		return core.ClaimedRelayCommand{
			CommandID:           "prompt-1",
			AcceptedOrdinal:     1,
			Command:             core.RelayCommandPrompt{Prompt: []acpschema.ContentBlock{user}},
			HiddenPromptContext: hiddenPromptContext,
			SteeringPrompt:      nil,
		}
	}
	promptOf := func(claimed core.ClaimedRelayCommand) []acpschema.ContentBlock {
		command := acpCommand(&claimed)
		prompt, ok := command.(CommandRequestPrompt)
		if !ok {
			t.Fatalf("%#v", command)
		}
		return prompt.Prompt
	}

	if got, want := promptOf(claimed(runtimeRelayString("<mj-project-memory>notes</mj-project-memory>"))), []acpschema.ContentBlock{
		hiddenContextBlock("<mj-project-memory>notes</mj-project-memory>"),
		user,
	}; !reflect.DeepEqual(got, want) {
		t.Fatalf("prompt = %#v, want %#v", got, want)
	}
	if got, want := promptOf(claimed(nil)), []acpschema.ContentBlock{user}; !reflect.DeepEqual(got, want) {
		t.Fatalf("prompt = %#v, want %#v", got, want)
	}
}

func TestDispatchBatchDoesNotOutgrowTheBoundedAcpCommandChannel(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	// A prompt and the cancel that targets it are both claimable at once,
	// so only the bounded channel limits the durable batch.
	runtimeRelayTestsSubmit(t, durable, "prompt-first", runtimeRelayTestsPrompt("running"))
	runtimeRelayTestsSubmit(t, durable, "cancel-second", core.RelayCommandCancel{})
	relay := NewRelayCell(durable)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(1)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})

	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-first", "running")
	runtimeRelayNoCommandWithin(t, commands, 25*time.Millisecond, "the second command was claimed beyond the channel's durable dispatch capacity")

	runtimeRelayEmit(t, events, core.RuntimeEventCommandRejected{
		RequestID: "prompt-first",
		Message:   "advance the bounded batch",
		Reason:    core.OutcomeReasonAdmissionRejected,
	})
	if id, ok := runtimeRelayCancelID(runtimeRelayCommandWithin(t, commands, time.Second)); !ok || id != "cancel-second" {
		t.Fatalf("expected the cancel, got %q", id)
	}
	runtimeRelayEmit(t, events, core.RuntimeEventCommandRejected{
		RequestID: "cancel-second",
		Message:   "test shutdown",
		Reason:    core.OutcomeReasonAdmissionRejected,
	})
	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

// Elicitation answers share the ACP channel with durable commands. They must
// not steal capacity between dispatch reserving slots and claiming work.
func TestOutOfBandSendsCannotStealReservedDispatchCapacity(t *testing.T) {
	const commandCapacity = 2
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	commands := newCommandChannel(commandCapacity)
	shellEvents := make(chan core.RuntimeEvent, 1)
	userShells := NewUserShellRegistry(temp, map[string]string{}, shellEvents)

	// Lock before starting dispatch itself. The coordinator also takes this
	// lock before dispatch, so racing its event loop can stall the test before
	// any permits are reserved.
	claiming := make(chan struct{})
	release := make(chan struct{})
	holder := core.SpawnBlocking(func() struct{} {
		held := relay.Lock()
		defer held.Unlock()
		runtimeRelayTestsSubmit(t, held.DurableRelay, "prompt-batched", runtimeRelayTestsPrompt("second turn"))
		runtimeRelayTestsSubmit(t, held.DurableRelay, "cancel-batched", core.RelayCommandCancel{})
		close(claiming)
		// Closing release on a failed assertion also releases the lock.
		<-release
		return struct{}{}
	})
	<-claiming
	dispatch := core.SpawnBlocking(func() error {
		return dispatchPending(relay, commands, map[string]core.RelayCommand{}, true, userShells)
	})
	releaseOnce := func() {
		select {
		case <-release:
		default:
			close(release)
		}
	}
	defer releaseOnce()
	waitUntil(t, func() bool { return commands.availableCapacity() == 0 },
		"dispatch never reserved transport capacity before claiming durable work")

	outOfBandAttempts := []error{
		commands.trySend(elicitationRequest()),
		commands.trySend(elicitationRequest()),
	}
	releaseOnce()
	runtimeRelayJoin(t, holder, "relay lock holder")
	runtimeRelayJoinOk(t, dispatch, "dispatch")

	for _, attempt := range outOfBandAttempts {
		if !errors.Is(attempt, errCommandChannelFull) {
			t.Fatalf("dispatch must reserve transport capacity before it claims durable work: %v", outOfBandAttempts)
		}
	}
	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-batched", "second turn")
	if id, ok := runtimeRelayCancelID(nextCommand(t, commands.receiver())); !ok || id != "cancel-batched" {
		t.Fatalf("expected the cancel, got %q", id)
	}
}

func TestCoordinatorDrainsEventsWhileTheAcpCommandChannelIsFull(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "prompt-batched", runtimeRelayTestsPrompt("queued turn"))
	runtimeRelayTestsSubmit(t, durable, "cancel-batched", core.RelayCommandCancel{})
	relay := NewRelayCell(durable)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(2)
	runtimeRelayMust(t, commands.trySend(elicitationRequest()))
	runtimeRelayMust(t, commands.trySend(elicitationRequest()))
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	runtimeRelayEmit(t, events, core.RuntimeEventWarning{Message: "out-of-band channel full"})
	waitUntil(t, func() bool { return recordedWarning(relay, "out-of-band channel full") },
		"out-of-band commands stopped the coordinator from draining ACP events")

	// Once ACP makes room, the same accepted prompt and cancel still dispatch.
	for range 2 {
		if command, ok := nextCommand(t, commands.receiver()).(CommandRequestResolveElicitation); !ok {
			t.Fatalf("expected an elicitation answer, got %#v", command)
		}
	}
	runtimeRelayMust(t, wakeDispatch(relay, wakes))
	waitUntil(t, func() bool { return runtimeRelayQueuedCommands(commands) == 2 },
		"the coordinator never dispatched the queued commands")
	runtimeRelayEmit(t, events, core.RuntimeEventWarning{Message: "durable channel full"})
	waitUntil(t, func() bool { return recordedWarning(relay, "durable channel full") },
		"durable commands stopped the coordinator from draining ACP events")
	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-batched", "queued turn")
	if id, ok := runtimeRelayCancelID(nextCommand(t, commands.receiver())); !ok || id != "cancel-batched" {
		t.Fatalf("expected the cancel, got %q", id)
	}

	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

// Hard-won: #987: a withdrawn saved model stranded startup before commands could repair it.
func TestASelectorHelAppliedForItselfIsDurableWithoutARelayCommand(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(2)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()

	// Recovering a session whose saved model the harness dropped applies a
	// selector nobody commanded. It must still reach durable state, or the
	// next restart replays the value that stranded the session.
	runtimeRelayEmit(t, events, core.RuntimeEventConfigApplied{
		RequestID:     "",
		Key:           "model",
		Value:         "recovered",
		ConfigOptions: []acpschema.SessionConfigOption{},
	})
	deadline := time.Now().Add(5 * time.Second)
	for runtimeRelayOperationalState(relay).Config["model"] != "recovered" {
		if !time.Now().Before(deadline) {
			t.Fatal("the recovered selector never became durable")
		}
		time.Sleep(5 * time.Millisecond)
	}

	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

func TestDifferentCommandTypesDispatchInAcceptanceOrder(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "config-first", core.RelayCommandSetConfig{Key: "model", Value: "before-prompt"})
	runtimeRelayTestsSubmit(t, durable, "prompt-second", runtimeRelayTestsPrompt("after config"))
	relay := NewRelayCell(durable)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(2)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})

	if id, ok := runtimeRelaySetConfigID(nextCommand(t, commands.receiver())); !ok || id != "config-first" {
		t.Fatalf("first dispatch = %q", id)
	}
	// The prompt waits for the configuration change accepted before it.
	runtimeRelayNoCommandWithin(t, commands, 25*time.Millisecond, "the prompt dispatched before the earlier configuration change finished")
	runtimeRelayEmit(t, events, core.RuntimeEventConfigApplied{
		RequestID:     "config-first",
		Key:           "model",
		Value:         "before-prompt",
		ConfigOptions: []acpschema.SessionConfigOption{},
	})
	assertPrompt(t, runtimeRelayCommandWithin(t, commands, time.Second), "prompt-second", "after config")

	runtimeRelayEmit(t, events, core.RuntimeEventCommandRejected{
		RequestID: "prompt-second",
		Message:   "test shutdown",
		Reason:    core.OutcomeReasonAdmissionRejected,
	})
	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

func TestRejectedPromptIsDurableAndDoesNotStallTheQueue(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "prompt-1", runtimeRelayTestsPrompt("first"))
	runtimeRelayTestsSubmit(t, durable, "prompt-2", runtimeRelayTestsPrompt("second"))
	relay := NewRelayCell(durable)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})

	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-1", "first")
	runtimeRelayEmit(t, events, core.RuntimeEventCommandRejected{
		RequestID: "prompt-1",
		Message:   "agent rejected prompt",
		Reason:    core.OutcomeReasonAdmissionRejected,
	})
	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-2", "second")
	rejected := false
	for _, event := range runtimeRelayLockedEvents(t, relay) {
		if observation, ok := event.Observation.(core.RelayObservationCommandRejected); ok &&
			observation.CommandID == "prompt-1" && observation.Message == "agent rejected prompt" {
			rejected = true
		}
	}
	if !rejected {
		t.Fatal("the rejection was not journaled")
	}

	runtimeRelayEmit(t, events, core.RuntimeEventCommandRejected{
		RequestID: "prompt-2",
		Message:   "test shutdown",
		Reason:    core.OutcomeReasonAdmissionRejected,
	})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

func TestSetSessionModeWaitsForIdleThenRecordsADurableOutcome(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})

	runtimeRelaySubmitLocked(t, relay, "prompt-1", runtimeRelayTestsPrompt("running"))
	runtimeRelayMust(t, wakes.trySend())
	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-1", "running")

	runtimeRelaySubmitLocked(t, relay, "session-mode-1", core.RelayCommandSetSessionMode{ModeID: "plan"})
	runtimeRelayMust(t, wakes.trySend())
	runtimeRelayNoCommandWithin(t, commands, 25*time.Millisecond, "the session mode dispatched before the active prompt finished")

	runtimeRelayEmit(t, events, core.RuntimeEventPromptFinished{
		Diagnostic: nil,
		RequestID:  "prompt-1",
		StopReason: "end_turn",
		Usage:      nil,
	})
	mode, ok := runtimeRelayCommandWithin(t, commands, time.Second).(CommandRequestSetSessionMode)
	if !ok || mode.RequestID != "session-mode-1" || mode.ModeID != "plan" {
		t.Fatalf("expected the session mode change, got %#v", mode)
	}
	runtimeRelayEmit(t, events, core.RuntimeEventSessionModeApplied{
		RequestID:     "session-mode-1",
		ModeID:        "plan",
		ConfigOptions: []acpschema.SessionConfigOption{},
		Modes:         nil,
	})
	waitForRelayState(t, relay, func(state core.RelayOperationalState) bool {
		return state.Config["mode"] == "plan"
	})

	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

func TestARejectedSessionModeChangeReportsTheFailureAndLeavesTheModeAlone(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		temp := t.TempDir()
		relay := runtimeRelayCell(t, temp, "1.0.0")
		events, eventRx := runtimeEventChannel()
		wakes := newDispatchWakeChannel()
		commands := newCommandChannel(4)
		coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
		defer stop()
		runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})

		runtimeRelaySubmitLocked(t, relay, "session-mode-1", core.RelayCommandSetSessionMode{ModeID: "plan"})
		runtimeRelayMust(t, wakes.trySend())
		mode, ok := nextCommand(t, commands.receiver()).(CommandRequestSetSessionMode)
		if !ok || mode.RequestID != "session-mode-1" || mode.ModeID != "plan" {
			t.Fatalf("expected the session mode change, got %#v", mode)
		}
		runtimeRelayEmit(t, events, core.RuntimeEventCommandRejected{
			RequestID: "session-mode-1",
			Message:   "set session mode to plan: no such mode",
			Reason:    core.OutcomeReasonAdmissionRejected,
		})
		waitForRelayState(t, relay, func(state core.RelayOperationalState) bool {
			_, present := state.Config["mode"]
			return !present
		})

		runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
		events.close()
		runtimeRelayJoinOk(t, coordinator, "relay coordinator")
	})
}

func TestExecutionModeRestorationWaitsForIdleAndRecordsTheConfirmedMode(t *testing.T) {
	request := core.RelayRequestSubmit{CommandID: "restore-mode", Command: core.RelayCommandRestoreExecutionMode{}}
	if core.RelayRequestSupportedAt(request, 27) {
		t.Fatal("protocol 27 cannot restore the execution mode")
	}
	if !core.RelayRequestSupportedAt(request, core.RelayProtocolVersion) {
		t.Fatal("the current protocol restores the execution mode")
	}

	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	runtimeRelaySubmitLocked(t, relay, "prompt-1", runtimeRelayTestsPrompt("running"))
	runtimeRelayMust(t, wakes.trySend())
	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-1", "running")
	runtimeRelaySubmitLocked(t, relay, "restore-mode", core.RelayCommandRestoreExecutionMode{})
	runtimeRelayMust(t, wakes.trySend())
	runtimeRelayNoCommandWithin(t, commands, 25*time.Millisecond, "the mode restoration dispatched before the active prompt finished")
	runtimeRelayEmit(t, events, core.RuntimeEventPromptFinished{
		Diagnostic: nil,
		RequestID:  "prompt-1",
		StopReason: "end_turn",
		Usage:      nil,
	})
	restore, ok := runtimeRelayCommandWithin(t, commands, time.Second).(CommandRequestRestoreExecutionMode)
	if !ok || restore.RequestID != "restore-mode" {
		t.Fatalf("expected the mode restoration, got %#v", restore)
	}
	runtimeRelayEmit(t, events, core.RuntimeEventConfigApplied{
		RequestID:     "restore-mode",
		Key:           "mode",
		Value:         "auto",
		ConfigOptions: []acpschema.SessionConfigOption{},
	})
	waitForRelayState(t, relay, func(state core.RelayOperationalState) bool {
		return state.Config["mode"] == "auto"
	})
	if !runtimeRelayCommandCompleted[core.RelayCommandOutcomeConfigured](runtimeRelayLockedEvents(t, relay), "restore-mode") {
		t.Fatal("the restoration has no configured outcome")
	}
	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
	reopened := runtimeRelayOpen(t, temp, "1.0.0")
	if got := reopened.OperationalState().Config["mode"]; got != "auto" {
		t.Fatalf("reopened mode = %q", got)
	}
}

func TestConfigCancelAndCloseCommandsHaveDurableTerminalOutcomes(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		temp := t.TempDir()
		relay := runtimeRelayCell(t, temp, "1.0.0")
		events, eventRx := runtimeEventChannel()
		wakes := newDispatchWakeChannel()
		commands := newCommandChannel(4)
		coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
		defer stop()
		runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})

		runtimeRelaySubmitLocked(t, relay, "config-1", core.RelayCommandSetConfig{Key: "model", Value: "test-model"})
		runtimeRelayMust(t, wakes.trySend())
		config, ok := nextCommand(t, commands.receiver()).(CommandRequestSetConfig)
		if !ok || config.RequestID != "config-1" || config.Key != "model" || config.Value != "test-model" {
			t.Fatalf("expected the configuration change, got %#v", config)
		}
		runtimeRelayEmit(t, events, core.RuntimeEventConfigApplied{
			RequestID:     "config-1",
			Key:           "model",
			Value:         "test-model",
			ConfigOptions: []acpschema.SessionConfigOption{},
		})
		waitForRelayState(t, relay, func(state core.RelayOperationalState) bool {
			return state.Config["model"] == "test-model"
		})

		runtimeRelaySubmitLocked(t, relay, "prompt-1", runtimeRelayTestsPrompt("running"))
		runtimeRelayMust(t, wakes.trySend())
		assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-1", "running")
		runtimeRelaySubmitLocked(t, relay, "cancel-1", core.RelayCommandCancel{})
		runtimeRelayMust(t, wakes.trySend())
		if id, ok := runtimeRelayCancelID(nextCommand(t, commands.receiver())); !ok || id != "cancel-1" {
			t.Fatalf("expected the cancel, got %q", id)
		}
		runtimeRelayEmit(t, events, core.RuntimeEventCancelApplied{RequestID: "cancel-1"})
		runtimeRelayEmit(t, events, core.RuntimeEventPromptFinished{
			Diagnostic: nil,
			RequestID:  "prompt-1",
			StopReason: "cancelled",
			Usage:      nil,
		})
		waitForRelayState(t, relay, func(state core.RelayOperationalState) bool {
			return state.Execution == core.RelayExecutionStateIdle && state.ActivePrompt == nil
		})

		runtimeRelaySubmitLocked(t, relay, "barrier-before-close", core.RelayCommandBeginCheckpoint{Reason: runtimeRelayString("close test")})
		runtimeRelayMust(t, wakes.trySend())
		waitForRelayState(t, relay, func(state core.RelayOperationalState) bool { return state.CheckpointReady != nil })
		expected := *runtimeRelayOperationalState(relay).CheckpointReady
		runtimeRelaySubmitLocked(t, relay, "close-01", core.RelayCommandClose{
			BarrierCommandID: "barrier-before-close",
			Expected:         expected,
		})
		runtimeRelaySubmitLocked(t, relay, "complete-before-close", core.RelayCommandCompleteCheckpoint{
			BarrierCommandID: "barrier-before-close",
		})
		runtimeRelayMust(t, wakes.trySend())
		closing, ok := nextCommand(t, commands.receiver()).(CommandRequestClose)
		if !ok || closing.RequestID != "close-01" {
			t.Fatalf("expected the close, got %#v", closing)
		}
		runtimeRelayEmit(t, events, core.RuntimeEventCloseApplied{RequestID: "close-01"})
		waitForRelayState(t, relay, func(state core.RelayOperationalState) bool {
			return state.Execution == core.RelayExecutionStateClosed
		})

		runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
		events.close()
		runtimeRelayJoinOk(t, coordinator, "relay coordinator")
		observations := runtimeRelayLockedEvents(t, relay)
		if !runtimeRelayCommandCompleted[core.RelayCommandOutcomeCancelled](observations, "cancel-1") {
			t.Fatal("the cancel has no cancelled outcome")
		}
		if !slices.ContainsFunc(observations, func(event core.RelayEvent) bool {
			_, ok := event.Observation.(core.RelayObservationClosed)
			return ok
		}) {
			t.Fatal("the close was not journaled")
		}
	})
}

// waitForRelayState waits until predicate holds for the relay's operational
// state, yielding between checks as Rust's yield_now loop does.
func waitForRelayState(t *testing.T, relay *RelayCell, predicate func(core.RelayOperationalState) bool) {
	t.Helper()
	testSupportWaitFor("relay state to reach the expected condition", func() struct{} {
		for !predicate(runtimeRelayOperationalState(relay)) {
			runtime.Gosched()
		}
		return struct{}{}
	})
}

// runtimeRelayConnected is the Connected event of a test agent (Rust:
// RuntimeEvent::Connected with default capabilities).
func runtimeRelayConnected(name, version string, steeringSupported bool) core.RuntimeEvent {
	protocol := acpschema.ProtocolVersion(1)
	return core.RuntimeEventConnected{
		AgentName:         &name,
		AgentVersion:      &version,
		ProtocolVersion:   &protocol,
		Capabilities:      &acpschema.AgentCapabilities{},
		AgentInfo:         &acpschema.Implementation{Name: name, Version: version},
		SteeringSupported: &steeringSupported,
	}
}

// runtimeRelayRecord records one runtime event and requires success.
func runtimeRelayRecord(t *testing.T, relay *RelayCell, inFlight map[string]core.RelayCommand, event core.RuntimeEvent) {
	t.Helper()
	if _, err := recordRuntimeEvent(relay, inFlight, event); err != nil {
		t.Fatal(err)
	}
}

func runtimeRelayHasObservation[O core.RelayObservation](events []core.RelayEvent) bool {
	return slices.ContainsFunc(events, func(event core.RelayEvent) bool {
		_, ok := event.Observation.(O)
		return ok
	})
}

func TestTypedAcpObservationsAreJournaled(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	inFlight := map[string]core.RelayCommand{}
	runtimeRelayRecord(t, relay, inFlight, runtimeRelayConnected("test-agent", "1", true))
	runtimeRelayRecord(t, relay, inFlight, runtimeRelayTestsAgentOutput("hello", "message-1"))

	events := runtimeRelayLockedEvents(t, relay)
	if !runtimeRelayHasObservation[core.RelayObservationAgentInitialized](events) {
		t.Fatal("the connection was not journaled")
	}
	if !runtimeRelayHasObservation[core.RelayObservationSessionUpdate](events) {
		t.Fatal("the session update was not journaled")
	}
	if got := runtimeRelayOperationalState(relay).SteeringSupported; got == nil || !*got {
		t.Fatalf("steering supported = %v", got)
	}
	{
		locked := relay.Lock()
		locked.ClearAcpReadiness()
		locked.Unlock()
	}
	if got := runtimeRelayOperationalState(relay).SteeringSupported; got != nil {
		t.Fatalf("steering supported after readiness cleared = %v", *got)
	}
	reopened := runtimeRelayOpen(t, temp, "1.0.0")
	if got := reopened.OperationalState().SteeringSupported; got != nil {
		t.Fatalf("reopened steering supported = %v", *got)
	}
}

// Hard-won: BrokkAi/muse-acp#382: Muse advertised only `steering.supported`,
// so its queued prompts waited for the turn to end.
func TestAdvertisedSteeringEnablesAutomaticSteering(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	inFlight := map[string]core.RelayCommand{}
	runtimeRelayRecord(t, relay, inFlight, runtimeRelayConnected("test-agent", "1", true))

	locked := relay.Lock()
	defer locked.Unlock()
	runtimeRelayTestsSubmit(t, locked.DurableRelay, "prompt-first", runtimeRelayTestsPrompt("first"))
	claimed, err := locked.ClaimPendingCommands(true)
	runtimeRelayMust(t, err)
	if len(claimed) != 1 {
		t.Fatalf("claimed = %#v", claimed)
	}
	runtimeRelayTestsSubmit(t, locked.DurableRelay, "prompt-second", runtimeRelayTestsPrompt("second"))
	claimed, err = locked.ClaimPendingCommands(true)
	runtimeRelayMust(t, err)
	if len(claimed) != 1 {
		t.Fatalf("a steering bridge must auto-steer the queued prompt: %#v", claimed)
	}
	steer := claimed[0]
	if !strings.HasPrefix(steer.CommandID, "auto-steer-") {
		t.Fatalf("command = %q", steer.CommandID)
	}
	if command, ok := steer.Command.(core.RelayCommandSteer); !ok || command.QueuedPromptID != "prompt-second" {
		t.Fatalf("command = %#v", steer.Command)
	}
}

// runtimeRelayClaimInto claims pending commands into inFlight.
func runtimeRelayClaimInto(t *testing.T, relay *DurableRelay, inFlight map[string]core.RelayCommand) {
	t.Helper()
	claimed, err := relay.ClaimPendingCommands(true)
	runtimeRelayMust(t, err)
	for _, command := range claimed {
		inFlight[command.CommandID] = command.Command
	}
}

// Hard-won: d2f26b4c6: recovery started before the bridge restart reached the transcript.
func TestHarnessRestartingInterruptsInFlightCommands(t *testing.T) {
	for _, control := range []string{"none", "cancel", "steer"} {
		temp := t.TempDir()
		durable := runtimeRelayOpen(t, temp, "1.0.0")
		_, err := durable.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
		runtimeRelayMust(t, err)
		durable.SetSteeringDeliveryMode(core.HarnessKindDsh, false)
		runtimeRelayTestsSubmit(t, durable, "prompt-1", runtimeRelayTestsPrompt("go"))
		inFlight := map[string]core.RelayCommand{}
		runtimeRelayClaimInto(t, durable, inFlight)
		switch control {
		case "cancel":
			runtimeRelayTestsSubmit(t, durable, "cancel-command", core.RelayCommandCancelTurnFor{ActivePromptID: "prompt-1"})
		case "steer":
			runtimeRelayTestsSubmit(t, durable, "queued-input", runtimeRelayTestsPrompt("new input"))
			runtimeRelayTestsSubmit(t, durable, "steer-command", core.RelayCommandSteer{
				ActivePromptID: "prompt-1",
				QueuedPromptID: "queued-input",
			})
		default:
			runtimeRelayTestsSubmit(t, durable, "queued-checkpoint", core.RelayCommandBeginCheckpoint{
				Reason: runtimeRelayString("wait for the current turn"),
			})
		}
		runtimeRelayClaimInto(t, durable, inFlight)
		relay := NewRelayCell(durable)
		runtimeRelayRecord(t, relay, inFlight, core.RuntimeEventHarnessRestarting{
			Message: "ACP bridge exited; reloading the native session",
		})
		if len(inFlight) != 0 {
			t.Fatalf("%s: in flight = %#v", control, inFlight)
		}
		locked := relay.Lock()
		events := runtimeRelayEvents(t, locked.DurableRelay)
		interrupted := slices.IndexFunc(events, func(event core.RelayEvent) bool {
			observation, ok := event.Observation.(core.RelayObservationCommandInterrupted)
			return ok && observation.CommandID == "prompt-1"
		})
		restarted := slices.IndexFunc(events, func(event core.RelayEvent) bool {
			_, ok := event.Observation.(core.RelayObservationSessionRestarted)
			return ok
		})
		if interrupted < 0 || restarted < 0 || interrupted >= restarted {
			t.Fatalf("%s: interrupted at %d, restarted at %d", control, interrupted, restarted)
		}
		if slices.ContainsFunc(events, func(event core.RelayEvent) bool {
			started, ok := event.Observation.(core.RelayObservationCommandStarted)
			return ok && core.IsRestartContinuation(started.CommandID)
		}) {
			t.Fatalf("%s: a restart continuation started before the session was configured", control)
		}
		claimed, err := locked.ClaimPendingCommands(false)
		runtimeRelayMust(t, err)
		if len(claimed) != 0 {
			t.Fatalf("%s: claimed before configuration: %#v", control, claimed)
		}
		locked.Unlock()
		runtimeRelayRecord(t, relay, inFlight, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
		locked = relay.Lock()
		claimed, err = locked.ClaimPendingCommands(true)
		runtimeRelayMust(t, err)
		if control == "cancel" {
			if len(claimed) != 0 {
				t.Fatalf("a pending cancel must survive restart ordering: %#v", claimed)
			}
		} else {
			if len(claimed) != 1 {
				t.Fatalf("%s: claimed = %#v", control, claimed)
			}
			if !strings.HasPrefix(claimed[0].CommandID, "auto-continue-restart-") {
				t.Fatalf("%s: claimed %q", control, claimed[0].CommandID)
			}
			projection := core.MaterializedSessionEmpty(runtimeRelayTestsSessionID)
			for _, event := range runtimeRelayEvents(t, locked.DurableRelay) {
				projected, err := transcript.ProjectRelayEvent(&projection, &event)
				runtimeRelayMust(t, err)
				runtimeRelayMust(t, transcript.ApplyCommittedProjectionEvent(&projection, &event, projected.Mutation))
			}
			if projection.ActiveTurn == nil || projection.ActiveTurn.CommandID != claimed[0].CommandID {
				t.Fatalf("%s: active turn = %#v", control, projection.ActiveTurn)
			}
			if _, ok := projection.Execution.(core.MaterializedExecutionStateRunning); !ok {
				t.Fatalf("%s: execution = %#v", control, projection.Execution)
			}
			if control == "steer" {
				state := locked.OperationalState()
				if state.QueuedPrompts[0].CommandID != "queued-input" {
					t.Fatalf("queued = %#v", state.QueuedPrompts)
				}
				if state.Steering == nil || state.Steering.Status != core.SteeringStatusUnconfirmed {
					t.Fatalf("steering = %#v", state.Steering)
				}
			}
		}
		locked.Unlock()
	}
}

func TestHarnessRestartingRetiresDeadQuestionsBeforeALiveCut(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "prompt-1", runtimeRelayTestsPrompt("go"))
	relay := NewRelayCell(durable)
	inFlight := map[string]core.RelayCommand{"prompt-1": runtimeRelayTestsPrompt("go")}

	// Hard-won: d9dcf7c: interruption exposed a cut with a dead question responder.
	question, err := core.ElicitationRequestFromACPParams("dead-responder", runtimeRelayJSON(t, map[string]any{
		"mode": "form", "sessionId": runtimeRelayTestsSessionID, "message": "Approve?", "requestedSchema": map[string]any{"type": "object", "properties": map[string]any{}},
	}))
	runtimeRelayMust(t, err)
	{
		owner := relay.Lock()
		_, err := owner.ClaimPendingCommands(true)
		runtimeRelayMust(t, err)
		_, err = owner.RecordObservation(core.RelayObservationElicitationRequested{Request: question})
		runtimeRelayMust(t, err)
		owner.Unlock()
	}
	locked := relay.Lock()
	cut := locked.ReserveLiveBaseline()
	locked.Unlock()
	if _, ok := cut.TryRecv(); ok {
		t.Fatal("the cut was taken while a turn ran")
	}
	runtimeRelayRecord(t, relay, inFlight, core.RuntimeEventHarnessRestarting{
		Message: "ACP bridge exited; reloading the native session",
	})

	if len(inFlight) != 0 {
		t.Fatalf("the in-flight prompt must be interrupted: %#v", inFlight)
	}
	events := runtimeRelayLockedEvents(t, relay)
	if !slices.ContainsFunc(events, func(event core.RelayEvent) bool {
		warning, ok := event.Observation.(core.RelayObservationWarning)
		return ok && strings.Contains(warning.Message, "reloading the native session")
	}) {
		t.Fatal("the restart warning was not journaled")
	}
	if !slices.ContainsFunc(events, func(event core.RelayEvent) bool {
		interrupted, ok := event.Observation.(core.RelayObservationCommandInterrupted)
		return ok && interrupted.CommandID == "prompt-1"
	}) {
		t.Fatal("the prompt was not interrupted")
	}
	interrupted := slices.IndexFunc(events, func(event core.RelayEvent) bool {
		_, ok := event.Observation.(core.RelayObservationCommandInterrupted)
		return ok
	})
	restarted := slices.IndexFunc(events, func(event core.RelayEvent) bool {
		_, ok := event.Observation.(core.RelayObservationSessionRestarted)
		return ok
	})
	if interrupted < 0 || restarted < 0 || interrupted >= restarted {
		t.Fatalf("interrupted at %d, restarted at %d", interrupted, restarted)
	}
	result, ok := cut.TryRecv()
	if !ok {
		t.Fatal("the live cut was not taken")
	}
	runtimeRelayMust(t, result.Err)
	if len(result.Baseline.Questions) != 0 {
		t.Fatalf("questions = %#v", result.Baseline.Questions)
	}
	if result.Baseline.State.OpenRequest != nil {
		t.Fatalf("open request = %#v", result.Baseline.State.OpenRequest)
	}
}

// Hard-won: de056ae: late TerminalStarted reopened a completed fallback card and blocked recovery.
func TestTerminalLifecycleJournalsAFallbackToolAndTailCappedOutput(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	inFlight := map[string]core.RelayCommand{}

	ordinalBeforeStart := runtimeRelayOperationalState(relay).LatestOrdinal
	runtimeRelayRecord(t, relay, inFlight, core.RuntimeEventTerminalStarted{
		TerminalID:  "term-1",
		Command:     "cargo test",
		StartedAtMs: 1_000,
	})
	operational := runtimeRelayOperationalState(relay)
	if operational.LatestOrdinal != ordinalBeforeStart+1 {
		t.Fatalf("latest ordinal = %d, want %d", operational.LatestOrdinal, ordinalBeforeStart+1)
	}
	if want := []core.ActiveAgentTerminal{{TerminalID: "term-1", Command: "cargo test", StartedAtMs: 1_000}}; !reflect.DeepEqual(operational.ActiveAgentTerminals, want) {
		t.Fatalf("starting a terminal remains visible operationally: %#v", operational.ActiveAgentTerminals)
	}
	if !slices.ContainsFunc(runtimeRelayLockedEvents(t, relay), func(event core.RelayEvent) bool {
		update, ok := event.Observation.(core.RelayObservationSessionUpdate)
		return ok && update.Update.Kind == acpschema.SessionUpdateKindToolCall &&
			core.IsFallbackTerminalToolCall(update.Update.ToolCall) && update.Update.ToolCall.Title == "cargo test"
	}) {
		t.Fatal("the fallback terminal card was not journaled")
	}

	// A build log the size of a real one: far past both the pipe buffer and
	// the journal cap, so only the tail can survive.
	var builder strings.Builder
	builder.WriteString("first line of the build log\n")
	for builder.Len() < 512*1024 {
		builder.WriteString("compiling something that says nothing useful\n")
	}
	builder.WriteString("error: the last line is the one that matters\n")
	output := builder.String()
	produced := len(output)

	locked := relay.Lock()
	cut := locked.ReserveLiveBaseline()
	locked.Unlock()
	if _, ok := cut.TryRecv(); ok {
		t.Fatal("the cut was taken while the terminal ran")
	}
	exitCode := uint32(101)
	runtimeRelayRecord(t, relay, inFlight, core.RuntimeEventTerminalClosed{
		TerminalID: "term-1",
		Output:     output,
		Truncated:  false,
		ExitCode:   &exitCode,
		Signal:     nil,
	})

	result, ok := cut.TryRecv()
	if !ok {
		t.Fatal("the live cut was not taken once the terminal closed")
	}
	runtimeRelayMust(t, result.Err)
	locked = relay.Lock()
	completedFrontier := locked.LatestOrdinal()
	locked.Unlock()
	if result.Baseline.State.LatestOrdinal != completedFrontier {
		t.Fatalf("cut at %d, frontier %d", result.Baseline.State.LatestOrdinal, completedFrontier)
	}
	runtimeRelayRecord(t, relay, inFlight, core.RuntimeEventTerminalStarted{
		TerminalID:  "term-1",
		Command:     "cargo test",
		StartedAtMs: 1_000,
	})
	locked = relay.Lock()
	if locked.LatestOrdinal() != completedFrontier {
		t.Fatalf("a late start was journaled: %d != %d", locked.LatestOrdinal(), completedFrontier)
	}
	if !locked.ToolsInFlight().IsEmpty() {
		t.Fatal("a late start reopened a tool")
	}
	if terminals := locked.OperationalState().ActiveAgentTerminals; len(terminals) != 0 {
		t.Fatalf("the provisional activity disappears as soon as the child exits: %#v", terminals)
	}
	events := runtimeRelayEvents(t, locked.DurableRelay)
	locked.Unlock()

	index := slices.IndexFunc(events, func(event core.RelayEvent) bool {
		_, ok := event.Observation.(core.RelayObservationTerminalOutput)
		return ok
	})
	if index < 0 {
		t.Fatal("the closed terminal is journaled")
	}
	event := events[index]
	terminal := event.Observation.(core.RelayObservationTerminalOutput)

	if terminal.TerminalID != "term-1" {
		t.Fatalf("terminal = %q", terminal.TerminalID)
	}
	if terminal.ExitCode == nil || *terminal.ExitCode != 101 {
		t.Fatalf("exit code = %v", terminal.ExitCode)
	}
	if terminal.Signal != nil {
		t.Fatalf("signal = %q", *terminal.Signal)
	}
	if !terminal.Truncated {
		t.Fatal("dropping the head must be disclosed")
	}
	if !strings.HasSuffix(terminal.Output, "error: the last line is the one that matters\n") {
		t.Fatal("the tail of the output is what says how the command ended")
	}
	if strings.Contains(terminal.Output, "first line of the build log") {
		t.Fatal("the head is what gets dropped, not the tail")
	}
	if !strings.Contains(terminal.Output, "[mj dropped") {
		t.Fatal("the drop is disclosed")
	}
	if len(terminal.Output) >= produced {
		t.Fatal("the journal copy is capped below what the terminal produced")
	}
	if encoded := runtimeRelayJSON(t, event); len(encoded) > core.RelayEventByteBudget {
		t.Fatalf("the capped event fits a replay page without further clamping: %d bytes", len(encoded))
	}
}

func TestAFastTerminalCannotBeResurrectedByALateStartEvent(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	inFlight := map[string]core.RelayCommand{}

	exitCode := uint32(0)
	runtimeRelayRecord(t, relay, inFlight, core.RuntimeEventTerminalClosed{
		TerminalID: "term-1",
		Output:     "",
		Truncated:  false,
		ExitCode:   &exitCode,
		Signal:     nil,
	})
	runtimeRelayRecord(t, relay, inFlight, core.RuntimeEventTerminalStarted{
		TerminalID:  "term-1",
		Command:     "true",
		StartedAtMs: 1_000,
	})

	if terminals := runtimeRelayOperationalState(relay).ActiveAgentTerminals; len(terminals) != 0 {
		t.Fatalf("active terminals = %#v", terminals)
	}
}

// runtimeRelayTestsAgentOutput is an agent message chunk as the ACP
// runtime forwards it (Rust's agent_output).
func runtimeRelayTestsAgentOutput(text, messageID string) core.RuntimeEvent {
	id := acpschema.MessageId(messageID)
	update, err := json.Marshal(acpschema.SessionUpdate{
		Kind: acpschema.SessionUpdateKindAgentMessageChunk,
		AgentMessageChunk: &acpschema.ContentChunk{
			Content:   runtimeRelayTextBlock(text),
			MessageID: &id,
		},
	})
	if err != nil {
		panic(err)
	}
	return core.RuntimeEventSessionUpdate{Update: update}
}

// claudeResult is the raw SDK result forwarded by the Claude adapter at a
// model boundary.
func claudeResult(origin string) core.ClaudeTurnResult {
	result, err := core.ClaudeTurnResultFromSDKMessage(map[string]any{
		"type": "result", "subtype": "success", "is_error": false, "num_turns": 2,
		"stop_reason": "end_turn", "result": "done",
		"usage":  map[string]any{"input_tokens": 100, "output_tokens": 7},
		"origin": map[string]any{"kind": origin},
	})
	if err != nil || result == nil {
		panic(fmt.Sprintf("not a Claude result: %v", err))
	}
	return *result
}

func harnessTurnOpen(relay *RelayCell) bool {
	return runtimeRelayOperationalState(relay).HarnessTurn != nil
}

// runtimeRelayClaudeTurnRelay opens a relay whose harness turns follow the
// Claude adapter.
func runtimeRelayClaudeTurnRelay(t *testing.T, root string) *RelayCell {
	t.Helper()
	durable := runtimeRelayOpen(t, root, "1.0.0")
	durable.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
	return NewRelayCell(durable)
}

func runtimeRelayQueued(state core.RelayOperationalState, commandID string) bool {
	return slices.ContainsFunc(state.QueuedPrompts, func(queued core.QueuedRelayPrompt) bool {
		return queued.CommandID == commandID
	})
}

// Claude Code re-invokes itself when a background task it started finishes.
// The coordinator keeps a prompt typed during that turn in mj's queue until
// the turn boundary, while a checkpoint barrier waits on the same turn state.
func TestASelfStartedTurnHoldsPromptPromotionAndCheckpointAdmission(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayClaudeTurnRelay(t, temp)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})

	// Agent output with no prompt in flight: the harness picked its work back
	// up on its own.
	runtimeRelayEmit(t, events, runtimeRelayTestsAgentOutput("the tests passed", "resumed-1"))
	waitUntil(t, func() bool { return harnessTurnOpen(relay) }, "agent output at idle did not open a turn")
	if got := runtimeRelayOperationalState(relay).Execution; got != core.RelayExecutionStateRunning {
		t.Fatalf("execution = %q", got)
	}

	// A prompt typed during that turn stays in the durable queue; it never
	// enters the adapter's queue, which cancellation may discard.
	runtimeRelaySubmitLocked(t, relay, "prompt-mid-turn", runtimeRelayTestsPrompt("also look at this"))
	runtimeRelayMust(t, wakes.trySend())
	waitUntil(t, func() bool {
		state := runtimeRelayOperationalState(relay)
		return state.HarnessTurn != nil && state.ActivePrompt == nil && runtimeRelayQueued(state, "prompt-mid-turn")
	}, "the prompt did not remain queued during the harness turn")
	runtimeRelayNoCommand(t, commands)

	// A turn-end marker settles the autonomous cycle, then promotion sends the
	// queued prompt exactly once.
	runtimeRelayEmit(t, events, core.RuntimeEventClaudeTurnResult{ClaudeTurnResult: claudeResult("task-notification")})
	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-mid-turn", "also look at this")
	waitUntil(t, func() bool { return !harnessTurnOpen(relay) }, "the cycle result did not settle the harness turn")

	runtimeRelayEmit(t, events, core.RuntimeEventPromptFinished{
		Diagnostic: nil,
		RequestID:  "prompt-mid-turn",
		StopReason: "end_turn",
		Usage:      nil,
	})
	waitUntil(t, func() bool { return runtimeRelayOperationalState(relay).ActivePrompt == nil }, "the queued prompt did not finish")
	runtimeRelayEmit(t, events, runtimeRelayTestsAgentOutput("and now the second task", "resumed-2"))
	waitUntil(t, func() bool { return harnessTurnOpen(relay) }, "the second cycle did not open a turn")

	// A checkpoint barrier waits for that turn instead of cutting through it.
	runtimeRelaySubmitLocked(t, relay, "barrier-mid-turn", core.RelayCommandBeginCheckpoint{Reason: runtimeRelayString("recovery copy")})
	runtimeRelayMust(t, wakes.trySend())
	// Long enough for the coordinator to run a claim cycle on that wake, so a
	// barrier it would have admitted is admitted before this assertion.
	time.Sleep(50 * time.Millisecond)
	if barrier := runtimeRelayOperationalState(relay).CheckpointBarrier; barrier != nil {
		t.Fatalf("a checkpoint barrier was admitted while the harness was working: %q", *barrier)
	}

	runtimeRelayEmit(t, events, core.RuntimeEventClaudeTurnResult{ClaudeTurnResult: claudeResult("task-notification")})
	waitUntil(t, func() bool {
		return runtimeRelayIs(runtimeRelayOperationalState(relay).CheckpointBarrier, "barrier-mid-turn")
	}, "the checkpoint barrier was not admitted once the harness turn settled")
	state := runtimeRelayOperationalState(relay)
	if state.HarnessTurn != nil {
		t.Fatalf("harness turn = %#v", state.HarnessTurn)
	}
	if state.Execution != core.RelayExecutionStateIdle {
		t.Fatalf("execution = %q", state.Execution)
	}
	if state.LastHarnessTurnStartedOrdinal == nil {
		t.Fatal("the harness turn's start was not recorded")
	}

	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

// A bridge replacement settles the old harness turn, but the queued prompt
// waits until the replacement session is configured before it is sent.
// Hard-won: #1225: a bridge restart could leave a prompt held by a turn that no longer existed.
func TestABridgeRestartReleasesAHeldPromptAfterSessionReadiness(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayClaudeTurnRelay(t, temp)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	runtimeRelayEmit(t, events, runtimeRelayTestsAgentOutput("working through a task", "restart-turn"))
	waitUntil(t, func() bool { return harnessTurnOpen(relay) }, "agent output did not open a harness turn")

	runtimeRelaySubmitLocked(t, relay, "prompt-after-restart", runtimeRelayTestsPrompt("continue after restart"))
	runtimeRelayMust(t, wakes.trySend())
	waitUntil(t, func() bool { return len(runtimeRelayOperationalState(relay).QueuedPrompts) == 1 },
		"the prompt was not accepted into the relay queue")
	runtimeRelayNoCommand(t, commands)

	runtimeRelayEmit(t, events, core.RuntimeEventHarnessRestarting{Message: "test bridge restart"})
	waitUntil(t, func() bool {
		state := runtimeRelayOperationalState(relay)
		return state.HarnessTurn == nil && state.ACPReady != nil && !*state.ACPReady
	}, "bridge restart did not close the old turn and readiness")
	runtimeRelayNoCommand(t, commands)

	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-after-restart", "continue after restart")
	runtime.Gosched()
	runtimeRelayNoCommand(t, commands)

	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

// A stopped harness exits its worker; recovery closes the old turn and sends
// the durable prompt only after the replacement worker configures ACP.
// Hard-won: #1225: a harness exit could otherwise leave a queued prompt held by a dead turn.
func TestHarnessExitDuringATurnReleasesHeldPromptAfterWorkerRecovery(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayClaudeTurnRelay(t, temp)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	runtimeRelayEmit(t, events, runtimeRelayTestsAgentOutput("working through a task", "exit-turn"))
	waitUntil(t, func() bool { return harnessTurnOpen(relay) }, "agent output did not open a harness turn")

	runtimeRelaySubmitLocked(t, relay, "prompt-after-exit", runtimeRelayTestsPrompt("continue after harness exit"))
	runtimeRelayMust(t, wakes.trySend())
	waitUntil(t, func() bool { return runtimeRelayQueued(runtimeRelayOperationalState(relay), "prompt-after-exit") },
		"the prompt did not stay in the durable queue")
	runtimeRelayNoCommand(t, commands)

	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
	if !harnessTurnOpen(relay) {
		t.Fatal("Stopped leaves recovery to close the turn")
	}

	recovered := runtimeRelayOpen(t, temp, "1.0.0")
	recovered.SetHarnessTurnPolicy(HarnessTurnPolicyClaudeAdapter)
	_, err := recovered.RecordObservation(core.RelayObservationSessionRestarted{})
	runtimeRelayMust(t, err)
	relay = NewRelayCell(recovered)
	events, eventRx = runtimeEventChannel()
	wakes = newDispatchWakeChannel()
	commands = newCommandChannel(4)
	coordinator, stopRecovered := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stopRecovered()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-after-exit", "continue after harness exit")
	runtime.Gosched()
	runtimeRelayNoCommand(t, commands)

	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "recovered relay coordinator")
}

// Esc's non-targeted cancellation interrupts a Claude cycle with no active
// prompt; the queued user prompt starts once that cycle reports its result.
// Hard-won: #1225: adapter cancellation could discard a prompt Hel had already shown as active.
func TestCancelTurnDuringAClaudeHarnessTurnDispatchesQueuedPromptOnce(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayClaudeTurnRelay(t, temp)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	runtimeRelayEmit(t, events, runtimeRelayTestsAgentOutput("Reviewing what the agent found", "follow-up-1"))
	waitUntil(t, func() bool { return harnessTurnOpen(relay) }, "agent output at idle did not open a turn")

	runtimeRelaySubmitLocked(t, relay, "prompt-after-stop", runtimeRelayTestsPrompt("continue with this"))
	{
		locked := relay.Lock()
		claimed, err := locked.ClaimPendingCommands(true)
		locked.Unlock()
		runtimeRelayMust(t, err)
		if len(claimed) != 0 {
			t.Fatalf("claimed during the harness turn: %#v", claimed)
		}
	}
	if queued := runtimeRelayOperationalState(relay).QueuedPrompts; len(queued) != 1 {
		t.Fatalf("Esc's prompt remains owned by the relay until cancellation settles: %#v", queued)
	}

	runtimeRelaySubmitLocked(t, relay, "stop-harness-turn", core.RelayCommandCancelTurn{})
	runtimeRelayMust(t, wakes.trySend())
	cancel, ok := nextCommand(t, commands.receiver()).(CommandRequestCancel)
	if !ok || cancel.SteeringPrompt != nil {
		t.Fatal("CancelTurn must reach session/cancel without adapter-queued text")
	}
	if cancel.RequestID != "stop-harness-turn" {
		t.Fatalf("cancel = %q", cancel.RequestID)
	}
	runtimeRelayEmit(t, events, core.RuntimeEventCancelApplied{RequestID: cancel.RequestID})
	interrupted := claudeResult("task-notification")
	interrupted.Subtype = "error_during_execution"
	interrupted.IsError = true
	runtimeRelayEmit(t, events, core.RuntimeEventClaudeTurnResult{ClaudeTurnResult: interrupted})
	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-after-stop", "continue with this")
	waitUntil(t, func() bool { return !harnessTurnOpen(relay) }, "the interrupted cycle's result did not end the turn")
	state := runtimeRelayOperationalState(relay)
	if state.Execution != core.RelayExecutionStateRunning {
		t.Fatalf("execution = %q", state.Execution)
	}
	if state.ActivePrompt == nil || state.ActivePrompt.CommandID != "prompt-after-stop" {
		t.Fatalf("active prompt = %#v", state.ActivePrompt)
	}
	runtime.Gosched()
	runtimeRelayNoCommand(t, commands)
	journal := runtimeRelayLockedEvents(t, relay)
	starts := 0
	for _, event := range journal {
		if started, ok := event.Observation.(core.RelayObservationCommandStarted); ok && started.CommandID == "prompt-after-stop" {
			starts++
		}
	}
	if starts != 1 {
		t.Fatalf("the queued prompt starts exactly once, started %d times", starts)
	}
	if !slices.ContainsFunc(journal, func(event core.RelayEvent) bool {
		completed, ok := event.Observation.(core.RelayObservationCommandCompleted)
		return ok && completed.CommandID == "stop-harness-turn"
	}) {
		t.Fatal("the Stop command completed")
	}

	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

// R4-2, replayed from the recorded session: `/model haiku` before any
// prompt made the Claude adapter answer with agent text, the worker opened a
// turn for it that nothing ever ended, and every stop was "completed" while
// the session stayed Running.
// Hard-won: c0f3bab: Claude's model-change notice opened a turn that no result could ever settle.
func TestAModelChangeAnswerOpensNoTurnAndAnUnansweredStopEndsOne(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		temp := t.TempDir()
		relay := runtimeRelayClaudeTurnRelay(t, temp)
		events, eventRx := runtimeEventChannel()
		wakes := newDispatchWakeChannel()
		commands := newCommandChannel(4)
		coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
		defer stop()
		runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})

		runtimeRelaySubmitLocked(t, relay, "model-haiku", core.RelayCommandSetConfig{Key: "model", Value: "haiku"})
		runtimeRelayMust(t, wakes.trySend())
		config, ok := nextCommand(t, commands.receiver()).(CommandRequestSetConfig)
		if !ok {
			t.Fatal("the model change must reach the prompt loop")
		}
		// The adapter publishes its notice before it answers the request.
		runtimeRelayEmit(t, events, runtimeRelayTestsAgentOutput(
			"**Auto mode unavailable:** the selected model does not support Auto mode; using Accept edits instead.",
			"auto-mode-notice",
		))
		runtimeRelayEmit(t, events, core.RuntimeEventConfigApplied{
			RequestID:     config.RequestID,
			Key:           "model",
			Value:         "haiku",
			ConfigOptions: []acpschema.SessionConfigOption{},
		})
		waitForRelayState(t, relay, func(state core.RelayOperationalState) bool {
			return state.Config["model"] == "haiku"
		})
		state := runtimeRelayOperationalState(relay)
		if state.HarnessTurn != nil {
			t.Fatal("the answer to a model change opened a turn")
		}
		if state.Execution != core.RelayExecutionStateIdle {
			t.Fatalf("execution = %q", state.Execution)
		}

		// Any turn Claude Code does not know about must still be stoppable.
		runtimeRelayEmit(t, events, runtimeRelayTestsAgentOutput("Text no cycle will ever settle", "orphan-1"))
		waitUntil(t, func() bool { return harnessTurnOpen(relay) }, "agent output at idle did not open a turn")
		runtimeRelaySubmitLocked(t, relay, "interrupt-turn", core.RelayCommandCancelTurn{})
		runtimeRelayMust(t, wakes.trySend())
		cancel, ok := nextCommand(t, commands.receiver()).(CommandRequestCancel)
		if !ok {
			t.Fatal("the interrupt must reach the prompt loop as a cancel")
		}
		runtimeRelayEmit(t, events, core.RuntimeEventCancelApplied{RequestID: cancel.RequestID})
		waitUntil(t, func() bool {
			locked := relay.Lock()
			defer locked.Unlock()
			_, ok := locked.UnansweredHarnessTurnStop()
			return ok
		}, "the applied stop was not recorded")
		if !harnessTurnOpen(relay) {
			t.Fatal("the harness may still answer the stop with its result")
		}
		time.Sleep(HarnessTurnStopGrace)
		waitUntil(t, func() bool { return !harnessTurnOpen(relay) }, "a stop the harness never answered left the turn running")
		if got := runtimeRelayOperationalState(relay).Execution; got != core.RelayExecutionStateIdle {
			t.Fatalf("execution = %q", got)
		}

		events.close()
		runtimeRelayJoinOk(t, coordinator, "relay coordinator")
	})
}

func TestCheckpointWaitsForCurrentSessionConfigurationThenStaysLocal(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "checkpoint-1", core.RelayCommandBeginCheckpoint{Reason: runtimeRelayString("test")})
	relay := NewRelayCell(durable)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(1)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()

	locked := relay.Lock()
	beforeConnection := locked.LatestOrdinal()
	locked.Unlock()
	protocol := acpschema.ProtocolVersion(1)
	name, version := "test-agent", "1"
	runtimeRelayEmit(t, events, core.RuntimeEventConnected{
		AgentName:         &name,
		AgentVersion:      &version,
		ProtocolVersion:   &protocol,
		Capabilities:      &acpschema.AgentCapabilities{},
		AgentInfo:         &acpschema.Implementation{Name: name, Version: version},
		SteeringSupported: nil,
	})
	waitForRelayState(t, relay, func(state core.RelayOperationalState) bool { return state.LatestOrdinal > beforeConnection })
	if barrier := runtimeRelayOperationalState(relay).CheckpointBarrier; barrier != nil {
		t.Fatal("checkpoint became ready before the current ACP session was configured")
	}
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})

	waitForRelayState(t, relay, func(state core.RelayOperationalState) bool {
		return runtimeRelayIs(state.CheckpointBarrier, "checkpoint-1")
	})
	runtimeRelayNoCommand(t, commands)
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

func TestCheckpointWaitsForAnInFlightConfigCommand(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "config-before", core.RelayCommandSetConfig{Key: "model", Value: "test-model"})
	runtimeRelayTestsSubmit(t, durable, "barrier-after-config", core.RelayCommandBeginCheckpoint{Reason: runtimeRelayString("after config")})
	relay := NewRelayCell(durable)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(2)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()

	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	if id, ok := runtimeRelaySetConfigID(nextCommand(t, commands.receiver())); !ok || id != "config-before" {
		t.Fatalf("expected the configuration change, got %q", id)
	}
	if barrier := runtimeRelayOperationalState(relay).CheckpointBarrier; barrier != nil {
		t.Fatal("checkpoint became ready before the ACP config command completed")
	}
	runtimeRelayEmit(t, events, core.RuntimeEventConfigApplied{
		RequestID:     "config-before",
		Key:           "model",
		Value:         "test-model",
		ConfigOptions: []acpschema.SessionConfigOption{},
	})
	// ConfigApplied carries the ACP response's complete configuration.
	// The coordinator must durably materialize its SessionConfigured
	// observation before admitting the waiting checkpoint.
	deadline := time.Now().Add(time.Second)
	for {
		locked := relay.Lock()
		state := locked.OperationalState()
		configuredEvents := 0
		for _, event := range runtimeRelayEvents(t, locked.DurableRelay) {
			if _, ok := event.Observation.(core.RelayObservationSessionConfigured); ok {
				configuredEvents++
			}
		}
		locked.Unlock()
		if runtimeRelayIs(state.CheckpointBarrier, "barrier-after-config") && configuredEvents == 2 {
			ready := state.CheckpointReady
			if ready == nil {
				t.Fatal("checkpoint is ready")
			}
			if ready.Ordinal != state.LatestOrdinal || ready.Digest != state.LatestDigest {
				t.Fatalf("ready at %#v, latest %d %s", ready, state.LatestOrdinal, state.LatestDigest)
			}
			break
		}
		if !time.Now().Before(deadline) {
			t.Fatal("the checkpoint was not admitted after the configuration materialized")
		}
		runtime.Gosched()
	}

	locked := relay.Lock()
	_, _, err := locked.CancelCheckpointBarrierOnDisconnect("barrier-after-config")
	locked.Unlock()
	runtimeRelayMust(t, err)
	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

// A checkpoint hands ACP dispatch back as soon as its archive exists, then
// keeps using the same connection for the transfer. When that connection
// finally drops, the released barrier is already terminal: cancelling it
// again would push a spurious interruption into the transcript.
func TestAReleasedCheckpointBarrierIsNotCancelledWhenItsConnectionDrops(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "released-barrier", core.RelayCommandBeginCheckpoint{Reason: runtimeRelayString("early release")})
	runtimeRelayTestsSubmit(t, durable, "queued-during-transfer", core.RelayCommandPrompt{
		Prompt: []acpschema.ContentBlock{runtimeRelayTextBlock("later")},
	})
	claimed, err := durable.ClaimPendingCommands(true)
	runtimeRelayMust(t, err)
	if len(claimed) != 1 {
		t.Fatalf("claimed = %#v", claimed)
	}
	_, err = durable.RecordCheckpointReady("released-barrier")
	runtimeRelayMust(t, err)
	floorBefore := durable.OperationalState().RecoveryFloorOrdinal
	relay := NewRelayCell(durable)

	client, server := runtimeRelayPair(t)
	wake := newDispatchWakeChannel()
	fatal, _ := fatalReports()
	served := core.Spawn(func() error {
		return serveClient(context.Background(), server, relay, wake, testCredentials(), nil, fatal)
	})
	response := runtimeRelayExchange(t, bufio.NewReader(client), client, core.RelayRequestEnvelope{
		RequestID:       "release-request",
		ProtocolVersion: core.RelayProtocolVersion,
		Request: core.RelayRequestSubmit{
			CommandID: "release-command",
			Command:   core.RelayCommandReleaseCheckpoint{BarrierCommandID: "released-barrier"},
		},
	})
	accepted, ok := runtimeRelayOk(t, response, "relay did not accept the release").(core.RelayResponsePayloadAccepted)
	if !ok || accepted.CommandID != "release-command" {
		t.Fatalf("relay did not accept the release: %#v", response.Body)
	}

	// Dropping the connection is exactly what the worker treats as the
	// controller disappearing.
	client.Close()
	runtimeRelayJoinOk(t, served, "serve the checkpoint connection")

	locked := relay.Lock()
	defer locked.Unlock()
	state := locked.OperationalState()
	if state.CheckpointBarrier != nil {
		t.Fatalf("barrier = %q", *state.CheckpointBarrier)
	}
	if state.RecoveryFloorOrdinal != floorBefore {
		t.Fatalf("recovery floor = %d, want %d", state.RecoveryFloorOrdinal, floorBefore)
	}
	if slices.ContainsFunc(runtimeRelayEvents(t, locked.DurableRelay), func(event core.RelayEvent) bool {
		interrupted, ok := event.Observation.(core.RelayObservationCommandInterrupted)
		return ok && interrupted.CommandID == "released-barrier"
	}) {
		t.Fatal("the dropped connection cancelled a barrier it had already released")
	}
	next, err := locked.ClaimPendingCommands(true)
	runtimeRelayMust(t, err)
	if len(next) != 1 || next[0].CommandID != "queued-during-transfer" {
		t.Fatalf("next = %#v", next)
	}
}

func TestCheckpointWakeRecordsAlreadyQueuedRuntimeEventsFirst(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(1)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	waitForRelayState(t, relay, func(state core.RelayOperationalState) bool { return state.LatestOrdinal >= 1 })

	runtimeRelayEmit(t, events, core.RuntimeEventWarning{Message: "queued before checkpoint wake"})
	runtimeRelaySubmitLocked(t, relay, "checkpoint-after-queued-event", core.RelayCommandBeginCheckpoint{Reason: runtimeRelayString("ordering test")})
	runtimeRelayMust(t, wakes.trySend())
	waitForRelayState(t, relay, func(state core.RelayOperationalState) bool { return state.CheckpointReady != nil })

	{
		locked := relay.Lock()
		state := locked.OperationalState()
		ready := state.CheckpointReady
		if ready == nil {
			t.Fatal("checkpoint is ready")
		}
		if ready.Ordinal != state.LatestOrdinal || ready.Digest != state.LatestDigest {
			t.Fatalf("ready at %#v, latest %d %s", ready, state.LatestOrdinal, state.LatestDigest)
		}
		var warningOrdinal *uint64
		for _, event := range runtimeRelayEvents(t, locked.DurableRelay) {
			if warning, ok := event.Observation.(core.RelayObservationWarning); ok && warning.Message == "queued before checkpoint wake" {
				ordinal := event.Ordinal
				warningOrdinal = &ordinal
				break
			}
		}
		locked.Unlock()
		if warningOrdinal == nil {
			t.Fatal("queued warning was recorded")
		}
		if *warningOrdinal >= ready.Ordinal {
			t.Fatalf("warning at %d, cut at %d", *warningOrdinal, ready.Ordinal)
		}
	}

	locked := relay.Lock()
	_, _, err := locked.CancelCheckpointBarrierOnDisconnect("checkpoint-after-queued-event")
	locked.Unlock()
	runtimeRelayMust(t, err)
	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

func TestCheckpointWakeIsNotStarvedByARuntimeEventFlood(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "checkpoint-during-event-flood", core.RelayCommandBeginCheckpoint{
		Reason: runtimeRelayString("event flood fairness"),
	})
	relay := NewRelayCell(durable)
	events := make(chan core.RuntimeEvent, 8)
	events <- core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}}
	for sequence := range 7 {
		events <- core.RuntimeEventWarning{Message: fmt.Sprintf("queued flood event %d", sequence)}
	}
	wakes := newDispatchWakeChannel()
	runtimeRelayMust(t, wakes.trySend())
	commands := newCommandChannel(1)
	floodStop := make(chan struct{})
	flood := core.Spawn(func() struct{} {
		sequence := uint64(7)
		for {
			select {
			case events <- core.RuntimeEventWarning{Message: fmt.Sprintf("live flood event %d", sequence)}:
			case <-floodStop:
				return struct{}{}
			}
			sequence++
		}
	})
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, events, wakes, commands)
	defer stop()

	deadline := time.Now().Add(2 * time.Second)
	for runtimeRelayOperationalState(relay).CheckpointReady == nil {
		if !time.Now().Before(deadline) {
			close(floodStop)
			t.Fatal("runtime event flood starved the checkpoint wake")
		}
		runtime.Gosched()
	}

	close(floodStop)
	runtimeRelayJoin(t, flood, "event flood")
	locked := relay.Lock()
	_, _, err := locked.CancelCheckpointBarrierOnDisconnect("checkpoint-during-event-flood")
	locked.Unlock()
	runtimeRelayMust(t, err)
	events <- core.RuntimeEventStopped{}
	close(events)
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

func TestCheckpointFreezesEffectfulCommandsSubmittedAfterTheBarrier(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "barrier-before-config", core.RelayCommandBeginCheckpoint{Reason: runtimeRelayString("freeze later work")})
	runtimeRelayTestsSubmit(t, durable, "config-after", core.RelayCommandSetConfig{Key: "model", Value: "later-model"})
	relay := NewRelayCell(durable)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(2)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()

	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	waitForRelayState(t, relay, func(state core.RelayOperationalState) bool {
		return runtimeRelayIs(state.CheckpointBarrier, "barrier-before-config")
	})
	runtimeRelayNoCommandWithin(t, commands, 25*time.Millisecond, "a post-barrier ACP command was dispatched before checkpoint completion")

	runtimeRelaySubmitLocked(t, relay, "complete-checkpoint", core.RelayCommandCompleteCheckpoint{BarrierCommandID: "barrier-before-config"})
	runtimeRelayMust(t, wakes.trySend())
	if id, ok := runtimeRelaySetConfigID(nextCommand(t, commands.receiver())); !ok || id != "config-after" {
		t.Fatalf("expected the configuration change, got %q", id)
	}

	runtimeRelayEmit(t, events, core.RuntimeEventCommandRejected{
		RequestID: "config-after",
		Message:   "test shutdown",
		Reason:    core.OutcomeReasonAdmissionRejected,
	})
	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

// A served request that cannot be persisted because teardown removed the
// worker root has to stop the daemon; answering on from memory would keep
// a closed session apparently alive.
func TestARemovedWorkerRootReportsAFatalFailureToTheDaemon(t *testing.T) {
	temp := t.TempDir()
	root := filepath.Join(temp, "worker-root")
	durable := runtimeRelayOpen(t, root, "1.0.0")
	_, err := durable.RecordObservation(core.RelayObservationWarning{Message: "before teardown"})
	runtimeRelayMust(t, err)
	through := durable.LatestOrdinal()
	digest := durable.LatestDigest()
	relay := NewRelayCell(durable)
	wake := newDispatchWakeChannel()
	fatal, fatalReceiver := fatalReports()
	server, client := runtimeRelayPair(t)
	served := core.Spawn(func() error {
		return serveClient(context.Background(), server, relay, wake, testCredentials(), nil, fatal)
	})
	runtimeRelayMust(t, os.RemoveAll(root))

	lines := bufio.NewReader(client)
	response := runtimeRelayExchange(t, lines, client, core.RelayRequestEnvelope{
		RequestID:       "acknowledge-after-teardown",
		ProtocolVersion: core.RelayProtocolVersion,
		Request: core.RelayRequestAcknowledge{
			ThroughOrdinal: through,
			ThroughDigest:  digest,
		},
	})
	if failure := runtimeRelayError(t, response, "acknowledging after teardown must fail"); failure.Code != core.RelayErrorCodeInternal {
		t.Fatalf("%#v", response.Body)
	}

	select {
	case report := <-fatalReceiver:
		if report == nil || !strings.Contains(report.Error(), "was removed") {
			t.Fatalf("%v", report)
		}
	case <-time.After(ioTimeout):
		t.Fatal("the daemon must be told")
	}
	if runtimeRelayExists(root) {
		t.Fatal("serving a request recreated the worker root")
	}

	client.Close()
	runtimeRelayJoinOk(t, served, "serve the relay connection")
}

// runtimeRelayCapturedLog collects log output, waking a waiter on every
// write (Rust's CapturedLog with its Notify).
type runtimeRelayCapturedLog struct {
	mu      sync.Mutex
	bytes   []byte
	changed chan struct{}
}

func newRuntimeRelayCapturedLog() *runtimeRelayCapturedLog {
	return &runtimeRelayCapturedLog{changed: make(chan struct{})}
}

func (l *runtimeRelayCapturedLog) Write(p []byte) (int, error) {
	l.mu.Lock()
	defer l.mu.Unlock()
	l.bytes = append(l.bytes, p...)
	close(l.changed)
	l.changed = make(chan struct{})
	return len(p), nil
}

// current is the text so far and a channel closed at the next write.
func (l *runtimeRelayCapturedLog) current() (string, <-chan struct{}) {
	l.mu.Lock()
	defer l.mu.Unlock()
	return string(l.bytes), l.changed
}

// runtimeRelayNotifyOne stores one pending wakeup, as tokio's
// Notify::notify_one does.
func runtimeRelayNotifyOne(signal chan struct{}) {
	select {
	case signal <- struct{}{}:
	default:
	}
}

// runtimeRelayNotified waits for a pending wakeup within ioTimeout.
func runtimeRelayNotified(t *testing.T, signal <-chan struct{}, description string) {
	t.Helper()
	select {
	case <-signal:
	case <-time.After(ioTimeout):
		t.Fatalf("timed out waiting for %s", description)
	}
}

// runtimeRelaySerializedIO is a finished runSerializedProjectMemoryIO.
type runtimeRelaySerializedIO struct {
	outcome  projectMemoryIOOutcome[struct{}]
	admitted bool
}

// Hard-won: 656a36132 review: cancelling a checkpoint left its barrier held until queued project-memory I/O finished
// Hard-won: 73b35b226 review: cancelled reads lost their storage failures and task panics.
func TestClientDisconnectDuringMemoryIoReleasesCheckpointAndRunsQueuedPrompt(t *testing.T) {
	scopedLogCaptureSeesEveryCallsite()
	for _, mode := range []string{
		"queued_read",
		"pipeline",
		"queued_write",
		"running_read",
		"started_write",
		"failed_write",
		"failed_read",
		"panicked_read",
	} {
		func() {
			queued := mode == "queued_read" || mode == "pipeline" || mode == "queued_write"
			mutatesMemory := mode == "queued_write" || mode == "started_write" || mode == "failed_write"
			temp := t.TempDir()
			relay := runtimeRelayCell(t, temp, "1.0.0")
			events, eventRx := runtimeEventChannel()
			wakes := newDispatchWakeChannel()
			commands := newCommandChannel(1)
			coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
			defer stop()
			runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
			server, client := runtimeRelayPair(t)
			endpoint := newProjectMemoryEndpoint(&core.ProjectMemoryLaunchConfig{
				HistorySocket:   nil,
				ProjectKey:      "project",
				Root:            filepath.Join(temp, "replica"),
				BaselineRoot:    filepath.Join(temp, "baseline"),
				RepositoryRoots: map[string]string{},
				MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
			})
			// Rust's std mpsc sender never blocks.
			releaseIO := make(chan error, 2)
			block := &projectMemoryIoBlock{
				started:           make(chan struct{}, 1),
				release:           releaseIO,
				panicAfterRelease: mode == "panicked_read",
			}
			var blockedIO *core.JoinHandle[runtimeRelaySerializedIO]
			if queued {
				io := endpoint.io
				blockedIO = core.Spawn(func() runtimeRelaySerializedIO {
					outcome, admitted := runSerializedProjectMemoryIO(context.Background(), io, false, func() (struct{}, error) {
						runtimeRelayNotifyOne(block.started)
						return struct{}{}, <-block.release
					})
					return runtimeRelaySerializedIO{outcome: outcome, admitted: admitted}
				})
			} else {
				endpoint.ioBlock = block
			}
			if queued {
				runtimeRelayNotified(t, block.started, "the queued operation to hold the project-memory I/O")
			}
			releasePending := true
			log := newRuntimeRelayCapturedLog()
			previousLogger := slog.Default()
			slog.SetDefault(slog.New(slog.NewTextHandler(log, nil)))
			defer slog.SetDefault(previousLogger)
			connection := defaultConnectionRuntime()
			connection.projectMemory = endpoint
			fatal, _ := fatalReports()
			serverTask := core.Spawn(func() error {
				return serveClientWithMemory(context.Background(), server, relay, wakes, testCredentials(), connection, fatal)
			})
			lines := bufio.NewReader(client)

			response := runtimeRelayExchange(t, lines, client, core.RelayRequestEnvelope{
				RequestID:       "begin-checkpoint",
				ProtocolVersion: core.RelayProtocolVersion,
				Request: core.RelayRequestSubmit{
					CommandID: "checkpoint-1",
					Command:   core.RelayCommandBeginCheckpoint{Reason: runtimeRelayString("test disconnect")},
				},
			})
			runtimeRelayOk(t, response, "begin checkpoint")
			deadline := time.Now().Add(time.Second)
			for !runtimeRelayIs(runtimeRelayOperationalState(relay).CheckpointBarrier, "checkpoint-1") {
				if !time.Now().Before(deadline) {
					t.Fatalf("%s: the checkpoint barrier was not admitted", mode)
				}
				runtime.Gosched()
			}

			response = runtimeRelayExchange(t, lines, client, core.RelayRequestEnvelope{
				RequestID:       "queue-prompt",
				ProtocolVersion: core.RelayProtocolVersion,
				Request: core.RelayRequestSubmit{
					CommandID: "prompt-1",
					Command:   runtimeRelayTestsPrompt("runs after disconnect"),
				},
			})
			runtimeRelayOk(t, response, "queue prompt")

			installed := core.ProjectMemorySnapshot{Files: map[string]string{"/MEMORY.md": "installed before dispatch\n"}}
			var memoryRequest core.RelayRequest
			if mutatesMemory {
				empty := core.ProjectMemorySnapshot{}
				tree, err := core.ProjectMemoryWireTreeEncode(installed, core.RelayProtocolVersion)
				runtimeRelayMust(t, err)
				memoryRequest = core.RelayRequestReplaceProjectMemoryTree{ExpectedReplica: empty.Version(), Tree: tree}
			} else {
				memoryRequest = core.RelayRequestProjectMemoryTransfer{Action: core.ProjectMemoryTransferActionBeginSnapshot{}}
			}
			runtimeRelaySend(t, client, core.RelayRequestEnvelope{
				RequestID:       "blocked-memory",
				ProtocolVersion: core.RelayProtocolVersion,
				Request:         memoryRequest,
			})
			runtimeRelayNotified(t, endpoint.ioRequested, "the project-memory request to reach its I/O")
			if !queued {
				runtimeRelayNotified(t, block.started, "the project-memory operation to start")
			}
			if mode == "pipeline" {
				runtimeRelayWrite(t, client, []byte("unexpected pipelined request\n"))
			}
			runtimeRelayMust(t, client.CloseWrite())
			client.Close()
			runtimeRelayNotified(t, endpoint.ioDisconnected, "the disconnect to reach the project-memory I/O")
			if mutatesMemory && !queued {
				select {
				case <-serverTask.Done():
					t.Fatalf("%s: disconnect detached an admitted write", mode)
				default:
				}
				if barrier := runtimeRelayOperationalState(relay).CheckpointBarrier; !runtimeRelayIs(barrier, "checkpoint-1") {
					t.Fatalf("%s: barrier = %v", mode, barrier)
				}
				runtimeRelayNoCommand(t, commands)
				var failure error
				if mode == "failed_write" {
					failure = errors.New("controlled write failure")
				}
				releaseIO <- failure
				releasePending = false
			}
			waitCtx, cancelWait := context.WithTimeout(context.Background(), 5*time.Second)
			served, joinErr := serverTask.Join(waitCtx)
			cancelWait()
			if errors.Is(joinErr, context.DeadlineExceeded) {
				t.Fatalf("%s: disconnect waited for project-memory I/O", mode)
			}
			if joinErr != nil {
				t.Fatalf("%s: %v", mode, joinErr)
			}
			switch mode {
			case "pipeline":
				if served == nil || !strings.Contains(served.Error(), "protocol error") {
					t.Fatalf("%s: served = %v", mode, served)
				}
			case "failed_write":
				if served == nil || !strings.Contains(served.Error(), "controlled write failure") {
					t.Fatalf("%s: served = %v", mode, served)
				}
			default:
				if served != nil {
					t.Fatalf("%s: served = %v", mode, served)
				}
			}

			if barrier := runtimeRelayOperationalState(relay).CheckpointBarrier; barrier != nil {
				t.Fatalf("%s: barrier = %q", mode, *barrier)
			}
			assertPrompt(t, runtimeRelayCommandWithin(t, commands, time.Second), "prompt-1", "runs after disconnect")
			if blockedIO != nil {
				select {
				case <-blockedIO.Done():
					t.Fatalf("%s: the queued operation finished before its release", mode)
				default:
				}
				releaseIO <- nil
				releasePending = false
				result := runtimeRelayJoin(t, blockedIO, "the queued project-memory operation")
				if !result.admitted || result.outcome.joinErr != nil || result.outcome.err != nil {
					t.Fatalf("%s: queued operation = %#v", mode, result)
				}
			} else if releasePending {
				// A running snapshot can finish after dispatch; wait for its permit
				// before removing its temporary filesystem fixture.
				if mode == "failed_read" {
					runtimeRelayMkdirAll(t, filepath.Join(temp, "replica"))
					runtimeRelayWriteFile(t, filepath.Join(temp, "replica/MEMORY.md"), []byte{0xff})
				}
				releaseIO <- nil
				releasePending = false
			}
			select {
			case endpoint.io <- struct{}{}:
				<-endpoint.io
			case <-time.After(ioTimeout):
				t.Fatalf("%s: the project-memory I/O permit was never returned", mode)
			}
			message := ""
			switch mode {
			case "failed_read":
				message = "read memory snapshot document /MEMORY.md"
			case "panicked_read":
				message = "controlled read panic"
			}
			if message != "" {
				testSupportWaitFor("cancelled memory read failure report", func() struct{} {
					for {
						text, changed := log.current()
						if strings.Contains(text, message) {
							return struct{}{}
						}
						<-changed
					}
				})
			}
			if mode == "failed_read" {
				runtimeRelayMust(t, os.Remove(filepath.Join(temp, "replica/MEMORY.md")))
			}
			expected := core.ProjectMemorySnapshot{}
			if mode == "started_write" {
				expected = installed
			}
			for _, directory := range []string{"replica", "baseline"} {
				store := core.NewProjectMemoryStore(filepath.Join(temp, directory))
				snapshot, err := store.Snapshot()
				runtimeRelayMust(t, err)
				if !maps.Equal(snapshot.Files, expected.Files) {
					t.Fatalf("%s: %s = %#v, want %#v", mode, directory, snapshot.Files, expected.Files)
				}
			}
			if !slices.ContainsFunc(runtimeRelayLockedEvents(t, relay), func(event core.RelayEvent) bool {
				interrupted, ok := event.Observation.(core.RelayObservationCommandInterrupted)
				return ok && interrupted.CommandID == "checkpoint-1"
			}) {
				t.Fatalf("%s: the checkpoint was not interrupted", mode)
			}

			runtimeRelayEmit(t, events, core.RuntimeEventCommandRejected{
				RequestID: "prompt-1",
				Message:   "test shutdown",
				Reason:    core.OutcomeReasonAdmissionRejected,
			})
			events.close()
			runtimeRelayJoinOk(t, coordinator, "relay coordinator")
		}()
	}
}

// runtimeRelayServeClient serves one connection with no ACP command
// channel and a wake whose receiver stays alive.
func runtimeRelayServeClient(t *testing.T, relay *RelayCell) (*net.UnixConn, *core.JoinHandle[error]) {
	t.Helper()
	server, client := runtimeRelayPair(t)
	wake := newDispatchWakeChannel()
	fatal, _ := fatalReports()
	task := core.Spawn(func() error {
		return serveClient(context.Background(), server, relay, wake, testCredentials(), nil, fatal)
	})
	return client, task
}

func TestRelayClientRejectsUnknownEnvelopeFieldsWithoutDisconnect(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	client, serverTask := runtimeRelayServeClient(t, relay)
	lines := bufio.NewReader(client)

	runtimeRelayWrite(t, client, []byte("{\"request_id\":\"retired\",\"protocol_version\":1,\"controller_store_id\":\"old\",\"request\":{\"method\":\"status\"}}\n"))
	rejected := runtimeRelayReadResponse(t, lines)
	if failure := runtimeRelayError(t, rejected, "an unknown envelope field must be rejected"); failure.Code != core.RelayErrorCodeInvalidRequest {
		t.Fatalf("%#v", rejected.Body)
	}

	accepted := runtimeRelayExchange(t, lines, client, core.RelayRequestEnvelope{
		RequestID:       "valid-after-invalid",
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         core.RelayRequestStatus{},
	})
	if _, ok := runtimeRelayOk(t, accepted, "status after a rejection").(core.RelayResponsePayloadStatus); !ok {
		t.Fatalf("%#v", accepted.Body)
	}

	runtimeRelayMust(t, client.CloseWrite())
	client.Close()
	runtimeRelayJoinOk(t, serverTask, "serve the relay connection")
}

func TestAnUnknownRelayMethodIsNamedInItsRejection(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	client, serverTask := runtimeRelayServeClient(t, relay)
	lines := bufio.NewReader(client)

	runtimeRelayWrite(t, client, []byte("{\"request_id\":\"future\",\"protocol_version\":1,\"request\":{\"method\":\"subscribe\",\"params\":{\"after_seq\":0}}}\n"))
	rejected := runtimeRelayReadResponse(t, lines)
	if rejected.RequestID != "future" {
		t.Fatalf("request = %q", rejected.RequestID)
	}
	failure := runtimeRelayError(t, rejected, "an unknown method must be rejected")
	if failure.Code != core.RelayErrorCodeInvalidRequest {
		t.Fatalf("code = %q", failure.Code)
	}
	if !strings.Contains(failure.Message, "does not support method") || !strings.Contains(failure.Message, "subscribe") {
		t.Fatalf("%s", failure.Message)
	}

	// The connection is a protocol boundary, not a casualty of the
	// rejection: the next request is still served.
	accepted := runtimeRelayExchange(t, lines, client, core.RelayRequestEnvelope{
		RequestID:       "after-unknown-method",
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         core.RelayRequestStatus{},
	})
	if _, ok := runtimeRelayOk(t, accepted, "status after a rejection").(core.RelayResponsePayloadStatus); !ok {
		t.Fatalf("%#v", accepted.Body)
	}

	runtimeRelayMust(t, client.CloseWrite())
	client.Close()
	runtimeRelayJoinOk(t, serverTask, "serve the relay connection")
}

func TestDisconnectBetweenCloseAndCheckpointCompletionDispatchesClose(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(1)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	server, client := runtimeRelayPair(t)
	fatal, _ := fatalReports()
	serverTask := core.Spawn(func() error {
		return serveClient(context.Background(), server, relay, wakes, testCredentials(), nil, fatal)
	})
	lines := bufio.NewReader(client)

	response := runtimeRelayExchange(t, lines, client, core.RelayRequestEnvelope{
		RequestID:       "begin-close-checkpoint",
		ProtocolVersion: core.RelayProtocolVersion,
		Request: core.RelayRequestSubmit{
			CommandID: "barrier-before-close",
			Command:   core.RelayCommandBeginCheckpoint{Reason: runtimeRelayString("disconnect race")},
		},
	})
	runtimeRelayOk(t, response, "begin checkpoint")
	waitForRelayState(t, relay, func(state core.RelayOperationalState) bool { return state.CheckpointReady != nil })
	expected := *runtimeRelayOperationalState(relay).CheckpointReady

	response = runtimeRelayExchange(t, lines, client, core.RelayRequestEnvelope{
		RequestID:       "queue-exact-close",
		ProtocolVersion: core.RelayProtocolVersion,
		Request: core.RelayRequestSubmit{
			CommandID: "close-after-barrier",
			Command: core.RelayCommandClose{
				BarrierCommandID: "barrier-before-close",
				Expected:         expected,
			},
		},
	})
	runtimeRelayOk(t, response, "queue close")

	runtimeRelayMust(t, client.CloseWrite())
	client.Close()
	runtimeRelayJoinOk(t, serverTask, "serve the relay connection")

	closing, ok := runtimeRelayCommandWithin(t, commands, time.Second).(CommandRequestClose)
	if !ok || closing.RequestID != "close-after-barrier" {
		t.Fatalf("expected the close, got %#v", closing)
	}
	runtimeRelayEmit(t, events, core.RuntimeEventCloseApplied{RequestID: "close-after-barrier"})
	waitForRelayState(t, relay, func(state core.RelayOperationalState) bool {
		return state.Execution == core.RelayExecutionStateClosed
	})
	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

func TestRelayV1ClientDisconnectDoesNotOwnCommandExecution(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(1)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	server, client := runtimeRelayPair(t)
	fatal, _ := fatalReports()
	serverTask := core.Spawn(func() error {
		return serveClient(context.Background(), server, relay, wakes, testCredentials(), nil, fatal)
	})
	runtimeRelaySend(t, client, core.RelayRequestEnvelope{
		RequestID:       "submit",
		ProtocolVersion: core.RelayProtocolVersion,
		Request: core.RelayRequestSubmit{
			CommandID: "prompt-1",
			Command:   runtimeRelayTestsPrompt("continues offline"),
		},
	})
	client.Close()
	// The response write may win or lose the close race; command
	// execution must not depend on it.
	runtimeRelayJoin(t, serverTask, "serve the relay connection")

	assertPrompt(t, runtimeRelayCommandWithin(t, commands, time.Second), "prompt-1", "continues offline")
	runtimeRelayEmit(t, events, core.RuntimeEventCommandRejected{
		RequestID: "prompt-1",
		Message:   "test shutdown",
		Reason:    core.OutcomeReasonAdmissionRejected,
	})
	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

// runtimeRelayClosedRelay opens a relay in root that a checkpoint barrier
// and a close already sealed.
func runtimeRelayClosedRelay(t *testing.T, root, barrierID, barrierReason, closeID string) *DurableRelay {
	t.Helper()
	durable := runtimeRelayOpen(t, root, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, barrierID, core.RelayCommandBeginCheckpoint{Reason: runtimeRelayString(barrierReason)})
	claimed, err := durable.ClaimPendingCommands(true)
	runtimeRelayMust(t, err)
	if len(claimed) != 1 || claimed[0].CommandID != barrierID {
		t.Fatalf("claimed = %#v", claimed)
	}
	if _, ok := claimed[0].Command.(core.RelayCommandBeginCheckpoint); !ok {
		t.Fatalf("claimed = %#v", claimed)
	}
	_, err = durable.RecordCheckpointReady(barrierID)
	runtimeRelayMust(t, err)
	expected := durable.OperationalState().CheckpointReady
	if expected == nil {
		t.Fatal("checkpoint is ready")
	}
	runtimeRelayTestsSubmit(t, durable, closeID, core.RelayCommandClose{
		BarrierCommandID: barrierID,
		Expected:         *expected,
	})
	runtimeRelayTestsSubmit(t, durable, "complete-checkpoint", core.RelayCommandCompleteCheckpoint{BarrierCommandID: barrierID})
	claimed, err = durable.ClaimPendingCommands(true)
	runtimeRelayMust(t, err)
	if len(claimed) != 1 || claimed[0].CommandID != closeID {
		t.Fatalf("claimed = %#v", claimed)
	}
	if _, ok := claimed[0].Command.(core.RelayCommandClose); !ok {
		t.Fatalf("claimed = %#v", claimed)
	}
	_, err = durable.RecordCommandCompleted(closeID, core.RelayCommandOutcomeClosed{})
	runtimeRelayMust(t, err)
	_, err = durable.RecordObservation(core.RelayObservationClosed{})
	runtimeRelayMust(t, err)
	return durable
}

func TestClosedRelayStaysAttachableAfterTheAcpRuntimeStops(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayClosedRelay(t, temp, "barrier-before-close", "close test", "close-command")
	relay := NewRelayCell(durable)

	socket := filepath.Join(temp, "closed-relay.sock")
	listener, err := net.ListenUnix("unix", &net.UnixAddr{Name: socket, Net: "unix"})
	runtimeRelayMust(t, err)
	wake := runtimeRelayDroppedWake()
	fatal, fatalReceiver := fatalReports()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	terminal := core.Spawn(func() error {
		return serveTerminalRelay(ctx, listener, relay, wake, testCredentials(), defaultConnectionRuntime(), fatal, fatalReceiver)
	})
	stream, err := net.Dial("unix", socket)
	runtimeRelayMust(t, err)
	defer stream.Close()
	response := runtimeRelayExchange(t, bufio.NewReader(stream), stream, core.RelayRequestEnvelope{
		RequestID:       "attach-closed-relay",
		ProtocolVersion: core.RelayProtocolVersion,
		Request: core.RelayRequestAttach{
			AfterOrdinal:     0,
			AfterDigest:      core.RelayEventGenesisDigest,
			KnownStateDigest: nil,
		},
	})
	attached, ok := runtimeRelayOk(t, response, "closed relay did not accept attach").(core.RelayResponsePayloadAttached)
	if !ok {
		t.Fatalf("closed relay did not accept attach: %#v", response.Body)
	}
	if attached.State.Execution != core.RelayExecutionStateClosed {
		t.Fatalf("execution = %q", attached.State.Execution)
	}

	runtimeRelayMust(t, stream.(*net.UnixConn).CloseWrite())
	select {
	case <-terminal.Done():
		t.Fatal("the terminal relay stopped serving before it was aborted")
	default:
	}
	runtimeRelayAbort(t, terminal, cancel)
}

// runtimeRelayConnectWhileRunning connects to root's control socket within
// limit, failing when the daemon stops first.
func runtimeRelayConnectWhileRunning[T any](t *testing.T, root string, daemon *core.JoinHandle[T], limit time.Duration, stopped string) net.Conn {
	t.Helper()
	deadline := time.Now().Add(limit)
	for {
		stream, err := net.Dial("unix", filepath.Join(root, "control.sock"))
		if err == nil {
			return stream
		}
		select {
		case <-daemon.Done():
			t.Fatalf("%s: %s", stopped, runtimeRelayTaskEnding(daemon))
		default:
		}
		if !time.Now().Before(deadline) {
			t.Fatalf("no control socket within %v: %v", limit, err)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

// runtimeRelayTaskEnding describes how a finished task ended, for a failure
// message: its result, or the panic it stopped with.
func runtimeRelayTaskEnding[T any](task *core.JoinHandle[T]) string {
	value, err := task.Join(context.Background())
	if err != nil {
		return err.Error()
	}
	return fmt.Sprintf("returned %v", value)
}

func TestDaemonRestartServesAClosedRelayWithoutStartingAcp(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayClosedRelay(t, temp, "barrier-before-restart-close", "closed restart test", "close-before-daemon-restart")
	closedFrontier := durable.LatestOrdinal()

	config := launchConfig("profile-home-that-must-not-be-used")
	config.BridgeCommand = filepath.Join(temp, "missing-acp-bridge")
	config.Cwd = temp
	root := temp
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	daemon := core.Spawn(func() error { return RunDaemon(ctx, root, config) })
	stream := runtimeRelayConnectWhileRunning(t, root, daemon, 10*time.Second, "closed relay daemon stopped during startup")
	defer stream.Close()
	response := runtimeRelayExchange(t, bufio.NewReader(stream), stream, core.RelayRequestEnvelope{
		RequestID:       "status-after-closed-restart",
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         core.RelayRequestStatus{},
	})
	status, ok := runtimeRelayOk(t, response, "closed relay did not serve status after restart").(core.RelayResponsePayloadStatus)
	if !ok {
		t.Fatalf("closed relay did not serve status after restart: %#v", response.Body)
	}
	if status.State.Execution != core.RelayExecutionStateClosed {
		t.Fatalf("execution = %q", status.State.Execution)
	}
	if status.State.LatestOrdinal != closedFrontier {
		t.Fatalf("latest ordinal = %d, want %d", status.State.LatestOrdinal, closedFrontier)
	}
	if runtimeRelayExists(filepath.Join(root, "acp-supervisor.json")) {
		t.Fatal("a closed relay started an ACP supervisor")
	}
	// Session teardown reads this file to stop the daemon before it
	// deletes the root out from under it.
	if pid := strings.TrimSpace(string(runtimeRelayReadFile(t, filepath.Join(root, core.WorkerPIDFile)))); pid != fmt.Sprint(os.Getpid()) {
		t.Fatalf("pidfile = %q, want %d", pid, os.Getpid())
	}

	runtimeRelayMust(t, stream.(*net.UnixConn).CloseWrite())
	select {
	case <-daemon.Done():
		t.Fatal("the daemon stopped before it was aborted")
	default:
	}
	runtimeRelayAbort(t, daemon, cancel)
}

func TestDaemonRestartRecordsOneMarkerAfterRecoveringInFlightWork(t *testing.T) {
	temp := t.TempDir()
	root := temp
	relay := runtimeRelayOpen(t, root, "1.0.0")
	runtimeRelayTestsSubmit(t, relay, "prompt-before-restart", runtimeRelayTestsPrompt("keep working"))
	claimed, err := relay.ClaimPendingCommands(true)
	runtimeRelayMust(t, err)
	if len(claimed) != 1 {
		t.Fatalf("claimed = %#v", claimed)
	}

	config := launchConfig(filepath.Join(temp, "profile"))
	config.Cwd = temp
	expectDaemonLaunchFailure(t, root, config)

	reopened := runtimeRelayOpen(t, root, "1.0.0")
	events := runtimeRelayEvents(t, reopened)
	interrupted := slices.IndexFunc(events, func(event core.RelayEvent) bool {
		observation, ok := event.Observation.(core.RelayObservationCommandInterrupted)
		return ok && observation.CommandID == "prompt-before-restart"
	})
	if interrupted < 0 {
		t.Fatal("restart recovery interrupts the old prompt")
	}
	markers := []int{}
	for index, event := range events {
		if _, ok := event.Observation.(core.RelayObservationSessionRestarted); ok {
			markers = append(markers, index)
		}
	}
	if len(markers) != 1 {
		t.Fatalf("restart markers at %v", markers)
	}
	if interrupted >= markers[0] {
		t.Fatalf("interrupted at %d, marker at %d", interrupted, markers[0])
	}
	if got := reopened.OperationalState().Execution; got != core.RelayExecutionStateIdle {
		t.Fatalf("execution = %q", got)
	}
}

// Hard-won: #1515: two relay locks in LaunchSpec field initializers deadlocked startup.
func TestFirstDaemonStartDoesNotRecordARestartMarker(t *testing.T) {
	temp := t.TempDir()
	root := temp
	config := launchConfig(filepath.Join(temp, "profile"))
	config.Cwd = temp
	expectDaemonLaunchFailure(t, root, config)

	reopened := runtimeRelayOpen(t, root, "1.0.0")
	if runtimeRelayHasObservation[core.RelayObservationSessionRestarted](runtimeRelayEvents(t, reopened)) {
		t.Fatal("a first start recorded a restart marker")
	}
}

// runtimeRelayWriteSeed writes a restored relay seed into root.
func runtimeRelayWriteSeed(t *testing.T, root string, seed core.RestoredRelaySeed) {
	t.Helper()
	runtimeRelayWriteFile(t, core.RestoredRelaySeedPath(root), runtimeRelayJSON(t, seed))
}

func TestRestoredRelaySeedRecordsARestartMarker(t *testing.T) {
	temp := t.TempDir()
	root := temp
	runtimeRelayWriteSeed(t, root, core.RestoredRelaySeed{
		CommandLedger:       nil,
		AssessmentState:     nil,
		NotificationStack:   nil,
		EventFrontier:       0,
		EventFrontierDigest: core.RelayEventGenesisDigest,
		QueuedPrompts:       []core.CanonicalQueuedPrompt{},
		AcceptedConfig:      map[string]string{},
		NativeSessionUnused: false,
		Mailbox:             nil,
	})
	config := launchConfig(filepath.Join(temp, "profile"))
	config.Cwd = temp
	expectDaemonLaunchFailure(t, root, config)

	reopened := runtimeRelayOpen(t, root, "1.0.0")
	markers := 0
	for _, event := range runtimeRelayEvents(t, reopened) {
		if _, ok := event.Observation.(core.RelayObservationSessionRestarted); ok {
			markers++
		}
	}
	if markers != 1 {
		t.Fatalf("restart markers = %d, want 1", markers)
	}
}

// I1-6: the restored relay starts with the model and effort the archived
// session accepted, so the first bridge start pins them.
func TestARestoredRelaySeedSuppliesTheAcceptedModelAndEffort(t *testing.T) {
	temp := t.TempDir()
	root := temp
	runtimeRelayWriteSeed(t, root, core.RestoredRelaySeed{
		CommandLedger:       nil,
		AssessmentState:     nil,
		NotificationStack:   nil,
		EventFrontier:       0,
		EventFrontierDigest: core.RelayEventGenesisDigest,
		QueuedPrompts:       []core.CanonicalQueuedPrompt{},
		AcceptedConfig: map[string]string{
			"model":  "opus[1m]",
			"effort": "high",
		},
		NativeSessionUnused: false,
		Mailbox:             nil,
	})
	relay := runtimeRelayOpen(t, root, "1.0.0")
	state := relay.OperationalState()
	accepted := core.AcceptedSessionConfigFromConfiguration(state.Config, state.ConfigOptions)
	if !runtimeRelayIs(accepted.Model, "opus[1m]") {
		t.Fatalf("model = %v", accepted.Model)
	}
	if !runtimeRelayIs(accepted.Effort, "high") {
		t.Fatalf("effort = %v", accepted.Effort)
	}
}

// I2-7: a session suspended before any prompt resumed on a relay restored
// from its checkpoint, with the native identity arriving in the launch
// configuration. The worker counted that identity as used because its own
// journal had not created it, so when Codex answered "thread not found" it
// refused to start a fresh thread. The checkpoint now says the session was
// never prompted, and that holds until the replacement opens.
// Hard-won: 0bb9439: a never-prompted session restored from checkpoint was wrongly treated as used native history.
func TestARestoredNeverPromptedSessionMayReplaceItsNativeSession(t *testing.T) {
	for _, nativeSessionUnused := range []bool{true, false} {
		temp := t.TempDir()
		root := temp
		runtimeRelayWriteSeed(t, root, core.RestoredRelaySeed{
			CommandLedger:       nil,
			AssessmentState:     nil,
			NotificationStack:   nil,
			EventFrontier:       7,
			EventFrontierDigest: strings.Repeat("b", 64),
			QueuedPrompts:       []core.CanonicalQueuedPrompt{},
			AcceptedConfig:      map[string]string{},
			NativeSessionUnused: nativeSessionUnused,
			Mailbox:             nil,
		})
		config := launchConfig(filepath.Join(temp, "profile"))
		config.NativeSessionID = runtimeRelayString("missing-thread")

		for range 2 {
			relay := runtimeRelayOpen(t, root, "1.0.0")
			runtimeRelayMust(t, recordImportedNativeIdentity(&config, relay))
			if resume := selectResumeSession(&config, relay); !runtimeRelayIs(resume, "missing-thread") {
				t.Fatalf("resume = %v", resume)
			}
			if got := relay.NativeSessionMayHaveHistory(); got != !nativeSessionUnused {
				t.Fatalf("unused=%v: may have history = %v", nativeSessionUnused, got)
			}
		}

		relay := runtimeRelayOpen(t, root, "1.0.0")
		_, err := relay.RecordObservation(core.RelayObservationSessionOpened{
			NativeSessionID:               "replacement",
			Resumed:                       false,
			NativeContinuityLost:          false,
			ReplacedUnusedNativeSessionID: nil,
		})
		runtimeRelayMust(t, err)
		if got := relay.NativeSessionMayHaveHistory(); got != !nativeSessionUnused {
			t.Fatalf("the replacement opened here and has not been used: unused=%v, may have history = %v", nativeSessionUnused, got)
		}
	}
}

// runtimeRelayModelOption is a select option in the model category.
func runtimeRelayModelOption(current string, choices ...[2]string) acpschema.SessionConfigOption {
	category := acpschema.SessionConfigOptionCategoryModel
	options := make([]acpschema.SessionConfigSelectOption, 0, len(choices))
	for _, choice := range choices {
		options = append(options, acpschema.SessionConfigSelectOption{Value: acpschema.SessionConfigValueId(choice[0]), Name: choice[1]})
	}
	return acpschema.SessionConfigOption{
		Kind:     acpschema.SessionConfigOptionKindSelect,
		ID:       "model",
		Name:     "Model",
		Category: &category,
		Select: &acpschema.SessionConfigSelect{
			CurrentValue: acpschema.SessionConfigValueId(current),
			Options:      options,
		},
	}
}

// runtimeRelayRecordAcceptedModel journals a configured model selector and
// its accepted value.
func runtimeRelayRecordAcceptedModel(t *testing.T, relay *DurableRelay, option acpschema.SessionConfigOption, model string) {
	t.Helper()
	_, err := relay.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{option}})
	runtimeRelayMust(t, err)
	_, err = relay.RecordObservation(core.RelayObservationConfigurationUpdated{Key: "model", Value: model})
	runtimeRelayMust(t, err)
}

// runtimeRelayPinnedCodexConfig is the CODEX_CONFIG the supervisor spec in
// root pins.
func runtimeRelayPinnedCodexConfig(t *testing.T, root string) map[string]any {
	t.Helper()
	spec, err := AcpSupervisorSpecRead(filepath.Join(root, "acp-supervisor.json"))
	runtimeRelayMust(t, err)
	var pinned map[string]any
	runtimeRelayMust(t, json.Unmarshal([]byte(spec.Environment["CODEX_CONFIG"]), &pinned))
	return pinned
}

// Codex rebuilds a resumed thread from the launch request, so the bridge has
// to start with the model this session accepted or the resume reports a
// mismatch. That pin belongs to the bridge's environment and nowhere else.
func TestACodexResumeLaunchesItsBridgeOnTheAcceptedModel(t *testing.T) {
	temp := t.TempDir()
	root := filepath.Join(temp, "relay")
	relay := runtimeRelayOpen(t, root, "1.0.0")
	runtimeRelayRecordAcceptedModel(t, relay, runtimeRelayModelOption("deepseek-flash",
		[2]string{"deepseek-flash", "Flash"},
		[2]string{"deepseek-v4-pro", "Pro"},
	), "deepseek-flash")

	config := launchConfig(filepath.Join(temp, "profile"))
	config.Cwd = temp
	config.BridgeCommand = stubRuntimeBridge(t, temp)
	config.Environment["CODEX_CONFIG"] = `{"default_permissions":"project","tui":"never"}`
	expectDaemonLaunchFailure(t, root, config)

	pinned := runtimeRelayPinnedCodexConfig(t, root)
	if pinned["model"] != "deepseek-flash" {
		t.Fatalf("model = %v", pinned["model"])
	}
	if pinned["default_permissions"] != "project" {
		t.Fatalf("default_permissions = %v", pinned["default_permissions"])
	}
	if pinned["tui"] != "never" {
		t.Fatalf("tui = %v", pinned["tui"])
	}
}

// Issue 1217: a sub-agent's Codex thread started on the profile default and
// was switched to its spawn model before the first turn, and that first
// request was refused over the connection Codex had prewarmed for the
// default. A new session's bridge now starts on the model it was created
// for, and a model the session accepted afterwards still wins.
func TestANewCodexSessionLaunchesItsBridgeOnItsCreationModel(t *testing.T) {
	pinnedModel := func(accepted *string) string {
		temp := t.TempDir()
		root := filepath.Join(temp, "relay")
		if accepted != nil {
			relay := runtimeRelayOpen(t, root, "1.0.0")
			runtimeRelayRecordAcceptedModel(t, relay, runtimeRelayModelOption(*accepted,
				[2]string{*accepted, "Accepted"},
				[2]string{"gpt-6-luna", "Luna"},
			), *accepted)
		}
		config := launchConfig(filepath.Join(temp, "profile"))
		config.Cwd = temp
		config.BridgeCommand = stubRuntimeBridge(t, temp)
		config.InitialModel = runtimeRelayString("gpt-6-luna")
		expectDaemonLaunchFailure(t, root, config)
		model, ok := runtimeRelayPinnedCodexConfig(t, root)["model"].(string)
		if !ok {
			t.Fatal("the bridge pins no model")
		}
		return model
	}

	if got := pinnedModel(nil); got != "gpt-6-luna" {
		t.Fatalf("new session model = %q", got)
	}
	if got := pinnedModel(runtimeRelayString("gpt-6-astra")); got != "gpt-6-astra" {
		t.Fatalf("accepted model = %q", got)
	}
}

// Hard-won: ac93f8ce9: the managed-child marker was re-read after the environment filter removed it, so no override applied
func TestAWorkerLaunchAppliesProfileOverridesOnlyToManagedSubagents(t *testing.T) {
	codexFeatures := []string{
		"apps",
		"plugins",
		"remote_plugin",
		"tool_suggest",
		"image_generation",
	}
	for _, harness := range []core.HarnessKind{core.HarnessKindCodex, core.HarnessKindClaude} {
		for _, managed := range []bool{false, true} {
			temp := t.TempDir()
			root := filepath.Join(temp, "worker")
			home := filepath.Join(root, "profile")
			runtimeRelayMkdirAll(t, home)
			claudeSettings := filepath.Join(home, "settings.json")
			codexConfig := filepath.Join(home, "config.toml")
			if harness == core.HarnessKindCodex {
				contents := "[features]\n"
				for _, feature := range codexFeatures {
					contents += feature + " = true\n"
				}
				runtimeRelayWriteFile(t, codexConfig, []byte(contents))
			} else {
				runtimeRelayWriteFile(t, claudeSettings, []byte(`{"promptSuggestionEnabled":true,"disableClaudeAiConnectors":false,"outputStyle":"Explanatory"}`))
			}

			config := launchConfig(home)
			config.Harness = harness
			config.Cwd = temp
			config.BridgeCommand = stubRuntimeBridge(t, temp)
			config.NoGithubCredentials = true
			config.Environment = map[string]string{"HOME": temp}
			harness.ConfigureStagedHomeEnvironment(home, config.Environment)
			if managed {
				config.Environment[core.SessionManagedSubagentEnv] = "1"
			}
			expectDaemonLaunchFailure(t, root, config)

			spec, err := AcpSupervisorSpecRead(filepath.Join(root, "acp-supervisor.json"))
			runtimeRelayMust(t, err)
			if got := spec.Environment[harness.HomeEnv()]; got != home {
				t.Fatalf("%s home = %q, want %q", harness.HomeEnv(), got, home)
			}
			if _, ok := spec.Environment[core.SessionManagedSubagentEnv]; ok {
				t.Fatal("the managed-child marker reached the bridge")
			}
			if harness == core.HarnessKindCodex {
				var parsed map[string]any
				_, err := toml.Decode(string(runtimeRelayReadFile(t, codexConfig)), &parsed)
				runtimeRelayMust(t, err)
				features, _ := parsed["features"].(map[string]any)
				for _, feature := range codexFeatures {
					value, ok := features[feature].(bool)
					if !ok || value != !managed {
						t.Fatalf("Codex %s, managed=%v: %v", feature, managed, features[feature])
					}
				}
			} else {
				var settings map[string]any
				runtimeRelayMust(t, json.Unmarshal(runtimeRelayReadFile(t, claudeSettings), &settings))
				if value, ok := settings["promptSuggestionEnabled"].(bool); !ok || value != !managed {
					t.Fatalf("promptSuggestionEnabled, managed=%v: %v", managed, settings["promptSuggestionEnabled"])
				}
				if value, ok := settings["disableClaudeAiConnectors"].(bool); !ok || value != managed {
					t.Fatalf("disableClaudeAiConnectors, managed=%v: %v", managed, settings["disableClaudeAiConnectors"])
				}
				wantStyle := "Explanatory"
				if managed {
					wantStyle = "Concise"
				}
				if value, ok := settings["outputStyle"].(string); !ok || value != wantStyle {
					t.Fatalf("outputStyle, managed=%v: %v", managed, settings["outputStyle"])
				}
				for name, want := range map[string]string{"DISABLE_AUTOUPDATER": "1", "CLAUDE_CODE_SILENT_TURN_REMINDER": "false"} {
					value, present := spec.Environment[name]
					if present != managed || (managed && value != want) {
						t.Fatalf("Claude bridge, managed=%v: %s = %q (present %v)", managed, name, value, present)
					}
				}
			}
		}
	}
}

// #1160: the controller leaves a ChatGPT Codex profile's API key settings out
// of the launch, but the worker adds the target's own login environment, which
// only it can see. The bridge's spec names the variables so the supervisor
// removes them from that as well, and never carries them itself.
// Hard-won: c4e2838: API-key environment inherited by the target reached a ChatGPT Codex profile.
func TestAWorkerPassesTheExcludedVariablesToItsBridge(t *testing.T) {
	temp := t.TempDir()
	root := filepath.Join(temp, "relay")
	config := launchConfig(filepath.Join(temp, "profile"))
	config.Cwd = temp
	config.BridgeCommand = stubRuntimeBridge(t, temp)
	config.ExcludedEnvironment = []string{"CODEX_API_KEY", "OPENAI_API_KEY"}
	// As a target setting would put it there, past the controller.
	config.Environment["OPENAI_API_KEY"] = "sk-svcacct-target"
	expectDaemonLaunchFailure(t, root, config)

	spec, err := AcpSupervisorSpecRead(filepath.Join(root, "acp-supervisor.json"))
	runtimeRelayMust(t, err)
	if want := []string{"CODEX_API_KEY", "OPENAI_API_KEY", core.FastinoKeyEnvironment, "MJ_REVIEW_GATE"}; !slices.Equal(spec.ExcludedEnvironment, want) {
		t.Fatalf("excluded = %q, want %q", spec.ExcludedEnvironment, want)
	}
	if _, ok := spec.Environment["OPENAI_API_KEY"]; ok {
		t.Fatal("the bridge spec carries OPENAI_API_KEY")
	}
}

// runtimeRelayHeldStdin is a supervisor's standard input whose writer the
// test holds (Rust: tokio::io::duplex with the other end kept alive).
func runtimeRelayHeldStdin(t *testing.T) (*io.PipeReader, *io.PipeWriter) {
	t.Helper()
	reader, writer := io.Pipe()
	t.Cleanup(func() { writer.Close() })
	return reader, writer
}

// The supervisor removes the excluded variables after it merges the login
// environment, so neither a launch setting nor a target's shell profile can
// hand the bridge an API key. Other variables pass through.
// Hard-won: c4e2838: a shell profile could reintroduce API-key variables after launch configuration filtering.
func TestTheAcpBridgeNeverSeesAnExcludedVariable(t *testing.T) {
	temp := t.TempDir()
	observed := filepath.Join(temp, "observed")
	script := fmt.Sprintf("printf '%%s|%%s|%%s\\n' \"${OPENAI_API_KEY-unset}\" \"${CODEX_API_KEY-unset}\" \"${KEPT-unset}\" > %s",
		core.PosixQuote(observed))
	spec := AcpSupervisorSpec{
		Command: "/bin/sh",
		Args:    []string{"-c", script},
		Environment: map[string]string{
			"OPENAI_API_KEY": "sk-svcacct-test",
			"CODEX_API_KEY":  "sk-test",
			"KEPT":           "yes",
		},
		ExcludedEnvironment: slices.Clone(core.CodexCredentialEnvironment[:]),
		Cwd:                 temp,
		HarnessLease:        nil,
		NodeLease:           nil,
	}
	supervisorStdin, _ := runtimeRelayHeldStdin(t)

	runtimeRelayMust(t, runACPSupervisorWithStreams(context.Background(), spec, supervisorStdin, io.Discard))

	if got := string(runtimeRelayReadFile(t, observed)); got != "unset|unset|yes\n" {
		t.Fatalf("observed = %q", got)
	}
}

// Hard-won: a4c6fe269: login environment composition could restore GitHub Enterprise tokens.
func TestSupervisorExcludesPrivateKeysAfterEnvironmentComposition(t *testing.T) {
	temp := t.TempDir()
	observed := filepath.Join(temp, "observed")
	config := launchConfig(filepath.Join(temp, "profile"))
	config.NoGithubCredentials = true
	config.Cwd = temp
	config.BridgeCommand = "/bin/sh"
	config.BridgeArgs = []string{
		"-c",
		fmt.Sprintf("printf '%%s|%%s|%%s|%%s|%%s' \"${GH_TOKEN-unset}\" \"${GITHUB_TOKEN-unset}\" \"${GH_ENTERPRISE_TOKEN-unset}\" \"${GITHUB_ENTERPRISE_TOKEN-unset}\" \"${MJ_JEV_FASTINO_KEY-unset}\" > %s",
			core.PosixQuote(observed)),
	}
	spec := AcpSupervisorSpecFrom(&config)
	path := filepath.Join(temp, "supervisor.json")
	runtimeRelayMust(t, spec.writeSpec(path))
	spec, err := AcpSupervisorSpecRead(path)
	runtimeRelayMust(t, err)
	// The supervisor composes this with the login environment before applying
	// its exclusions; tokens from either source must be absent from the child.
	for _, name := range core.GithubAuthenticationEnvironment {
		spec.Environment[name] = "reintroduced-secret"
	}
	spec.Environment[core.FastinoKeyEnvironment] = "fastino-private"
	supervisorStdin, _ := runtimeRelayHeldStdin(t)
	runtimeRelayMust(t, runACPSupervisorWithStreams(context.Background(), spec, supervisorStdin, io.Discard))
	if got := string(runtimeRelayReadFile(t, observed)); got != "unset|unset|unset|unset|unset" {
		t.Fatalf("observed = %q", got)
	}
}

func TestAcpSupervisorNoticesChildExitWhileADescendantHoldsStdoutOpen(t *testing.T) {
	temp := t.TempDir()
	spec := AcpSupervisorSpec{
		Command:             "/bin/sh",
		Args:                []string{"-c", "sleep 30 & exit 17"},
		Environment:         map[string]string{},
		ExcludedEnvironment: []string{},
		Cwd:                 temp,
		HarnessLease:        nil,
		NodeLease:           nil,
	}
	supervisorStdin, _ := runtimeRelayHeldStdin(t)

	task := core.Spawn(func() error {
		return runACPSupervisorWithStreams(context.Background(), spec, supervisorStdin, io.Discard)
	})
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	err, joinErr := task.Join(ctx)
	if joinErr != nil {
		t.Fatalf("the supervisor must observe the bridge process itself: %v", joinErr)
	}
	if err == nil {
		t.Fatal("the bridge exits unsuccessfully")
	}
	if !strings.Contains(err.Error(), "exit status: 17") {
		t.Fatalf("unexpected supervisor error: %v", err)
	}
}

// runtimeRelayDescriptorsOf duplicates every descriptor of this process
// that refers to path, as files a child can inherit.
func runtimeRelayDescriptorsOf(t *testing.T, path string) []*os.File {
	t.Helper()
	target, err := os.Stat(path)
	runtimeRelayMust(t, err)
	entries, err := os.ReadDir("/dev/fd")
	runtimeRelayMust(t, err)
	var files []*os.File
	for _, entry := range entries {
		var fd int
		if _, err := fmt.Sscanf(entry.Name(), "%d", &fd); err != nil {
			continue
		}
		info, err := os.Stat(fmt.Sprintf("/dev/fd/%d", fd))
		if err != nil || !os.SameFile(info, target) {
			continue
		}
		duplicate, err := syscall.Dup(fd)
		runtimeRelayMust(t, err)
		files = append(files, os.NewFile(uintptr(duplicate), path))
	}
	return files
}

// Hard-won: 68ad78d60: an unrelated fork kept the supervisor lease locked until exec.
//
// Rust holds an unrelated child between fork and exec, where it still has
// the supervisor's lease descriptor. Go cannot run code in that window, so
// the unrelated child here inherits a duplicate of the same descriptor
// outright and keeps it until the test releases it: the same open lock
// description outliving the supervisor.
func TestAcpSupervisorHoldsTheManagedNodeLeaseForTheProcessTree(t *testing.T) {
	temp := t.TempDir()
	leasePath := filepath.Join(temp, "node/.lease")
	runtimeRelayMkdirAll(t, filepath.Dir(leasePath))
	ready := filepath.Join(temp, "ready")
	script := fmt.Sprintf("touch %s; read line", core.PosixQuote(ready))
	spec := AcpSupervisorSpec{
		Command:             "/bin/sh",
		Args:                []string{"-c", script},
		Environment:         map[string]string{},
		ExcludedEnvironment: []string{},
		Cwd:                 temp,
		HarnessLease:        nil,
		NodeLease:           &leasePath,
	}
	supervisorStdin, heldStdin := runtimeRelayHeldStdin(t)
	task := core.Spawn(func() error {
		return runACPSupervisorWithStreams(context.Background(), spec, supervisorStdin, io.Discard)
	})

	started := testSupportWaitFor("the supervised child to start", func() bool {
		for !runtimeRelayExists(ready) {
			select {
			case <-task.Done():
				return false
			default:
			}
			time.Sleep(10 * time.Millisecond)
		}
		return true
	})
	if !started {
		t.Fatalf("the supervisor stopped before its child started: %s", runtimeRelayTaskEnding(task))
	}
	contender, err := os.OpenFile(leasePath, os.O_RDWR, 0)
	runtimeRelayMust(t, err)
	if err := syscall.Flock(int(contender.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); !errors.Is(err, syscall.EWOULDBLOCK) {
		contender.Close()
		t.Fatalf("the supervisor must hold the managed Node.js lease while the child runs: %v", err)
	}
	contender.Close()

	// Keep an unrelated child holding an inherited copy of the supervisor's
	// lease descriptor.
	inherited := runtimeRelayDescriptorsOf(t, leasePath)
	if len(inherited) == 0 {
		t.Fatal("the supervisor holds no lease descriptor")
	}
	unrelated := exec.Command("/bin/sh", "-c", "cat >/dev/null")
	unrelated.ExtraFiles = inherited
	unrelatedInput, err := unrelated.StdinPipe()
	runtimeRelayMust(t, err)
	runtimeRelayMust(t, unrelated.Start())
	for _, file := range inherited {
		file.Close()
	}

	heldStdin.Close()
	stopCtx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	stopped, joinErr := task.Join(stopCtx)
	if joinErr != nil {
		unrelatedInput.Close()
		unrelated.Wait()
		t.Fatalf("supervisor did not stop after its input closed: %v", joinErr)
	}
	if stopped != nil {
		unrelatedInput.Close()
		unrelated.Wait()
		t.Fatal(stopped)
	}

	released, err := os.OpenFile(leasePath, os.O_RDWR, 0)
	runtimeRelayMust(t, err)
	lockErr := syscall.Flock(int(released.Fd()), syscall.LOCK_EX|syscall.LOCK_NB)
	released.Close()
	// Probe while the inherited descriptor is still held, then release and
	// reap the fixture even when the assertion is going to fail.
	unrelatedInput.Close()
	if err := unrelated.Wait(); err != nil {
		t.Fatalf("the unrelated child failed: %v", err)
	}
	if lockErr != nil {
		t.Fatalf("the supervisor released the Node.js lease: %v", lockErr)
	}
}

func TestAcpBridgeKeepsTheConfiguredGithubWrapperAndDropsInheritedTokens(t *testing.T) {
	temp := t.TempDir()
	bin := filepath.Join(temp, "bin")
	runtimeRelayMust(t, os.Mkdir(bin, 0o755))
	wrapper := filepath.Join(bin, "gh")
	core.InstallFakeCommand(bin, "gh", "#!/bin/sh\nexit 0\n")
	observed := filepath.Join(temp, "observed")
	script := fmt.Sprintf("printf '%%s\\n%%s|%%s\\n' \"$(command -v gh)\" \"${GH_TOKEN-unset}\" \"${GITHUB_TOKEN-unset}\" > %s",
		core.PosixQuote(observed))
	spec := AcpSupervisorSpec{
		Command: "/bin/sh",
		Args:    []string{"-c", script},
		Environment: map[string]string{
			"PATH":         bin + ":/usr/bin:/bin",
			"GH_TOKEN":     "stale-token",
			"GITHUB_TOKEN": "also-stale",
		},
		ExcludedEnvironment: []string{},
		Cwd:                 temp,
		HarnessLease:        nil,
		NodeLease:           nil,
	}
	supervisorStdin, _ := runtimeRelayHeldStdin(t)

	runtimeRelayMust(t, runACPSupervisorWithStreams(context.Background(), spec, supervisorStdin, io.Discard))

	if got, want := string(runtimeRelayReadFile(t, observed)), wrapper+"\nunset|unset\n"; got != want {
		t.Fatalf("observed = %q, want %q", got, want)
	}
}

// Opening the relay recovers its journal in place, so a second daemon has
// to detect the live one before it can rewrite files the first is using.
func TestALiveWorkerStopsASecondDaemonBeforeItTouchesDurableState(t *testing.T) {
	temp := t.TempDir()
	root := temp
	relay := runtimeRelayOpen(t, root, "1.0.0")
	_, err := relay.RecordObservation(core.RelayObservationWarning{Message: "recorded by the live worker"})
	runtimeRelayMust(t, err)

	// A torn tail is exactly what a startup recovery would rewrite.
	journal := filepath.Join(root, core.RelayJournalDir, "active.jsonl")
	torn := append(runtimeRelayReadFile(t, journal), []byte("{\"ordinal\":2,\"truncated\"")...)
	runtimeRelayWriteFile(t, journal, torn)
	exitRecord := filepath.Join(root, "worker-exit.json")
	runtimeRelayWriteFile(t, exitRecord, []byte("{\"reason\":\"earlier life\"}"))
	live, err := net.ListenUnix("unix", &net.UnixAddr{Name: filepath.Join(root, "control.sock"), Net: "unix"})
	runtimeRelayMust(t, err)
	defer live.Close()

	err = RunDaemon(context.Background(), root, launchConfig("/profile"))

	if err == nil || !strings.Contains(err.Error(), "a worker is already running") {
		t.Fatalf("%v", err)
	}
	if got := runtimeRelayReadFile(t, journal); !bytes.Equal(got, torn) {
		t.Fatal("the second daemon recovered a live worker's journal")
	}
	if !runtimeRelayExists(exitRecord) {
		t.Fatal("the live worker's exit record was cleared")
	}
	if runtimeRelayExists(filepath.Join(root, core.WorkerPIDFile)) {
		t.Fatal("the second daemon claimed a root it does not own")
	}
}

// Teardown needs the PID of the daemon that owns the root right now, not
// of one that died earlier.
func TestTheWorkerPidfileReplacesAPreviousDaemonsClaim(t *testing.T) {
	temp := t.TempDir()
	pidfile := filepath.Join(temp, core.WorkerPIDFile)
	runtimeRelayWriteFile(t, pidfile, []byte("999999999\n"))

	runtimeRelayMust(t, writeWorkerPidfile(temp, uint32(os.Getpid())))

	if got := strings.TrimSpace(string(runtimeRelayReadFile(t, pidfile))); got != fmt.Sprint(os.Getpid()) {
		t.Fatalf("pidfile = %q", got)
	}
}

func TestOversizedResponseIsRejectedBeforeWriting(t *testing.T) {
	server, _ := runtimeRelayPair(t)
	response := core.RelayResponseEnvelope{
		RequestID:       "oversized",
		ProtocolVersion: core.RelayProtocolVersion,
		Body: core.RelayResponseBodyError{Error: core.RelayProtocolError{
			Code:      core.RelayErrorCodeInternal,
			Message:   strings.Repeat("x", core.MaxFrameBytes),
			Retryable: false,
			Detail:    nil,
		}},
	}

	err := writeResponse(server, &response)
	if err == nil || !strings.Contains(err.Error(), "response frame is too large") {
		t.Fatalf("%v", err)
	}
}

// Hard-won: 390e88f94: the Go worker answered command_retention_status with
// "operation_ids":null, which the Rust daemon refused, so no session started.
// The frames were recorded from the Rust worker at 3dde0c11b (a fresh OpenCode
// session's control socket); a fresh Go relay must answer each request with
// the same bytes.
func TestAFreshRelayAnswersAsTheRustWorkerDid(t *testing.T) {
	recorded := runtimeRelayReadFile(t, filepath.Join("testdata", "rust_worker_exchange", "fresh_relay_frames.txt"))
	frames := strings.Split(strings.TrimSuffix(string(recorded), "\n"), "\n")
	if len(frames) == 0 || len(frames)%2 != 0 {
		t.Fatalf("the recording holds %d frames, not request and response pairs", len(frames))
	}
	relay := runtimeRelayCell(t, t.TempDir(), "2.74.0")
	for pair := 0; pair < len(frames); pair += 2 {
		request, want := frames[pair], frames[pair+1]
		decoded, known := core.DecodeRelayRequest([]byte(request)).(core.DecodedRelayRequestKnown)
		if !known {
			t.Fatalf("the Go relay cannot decode %s", request)
		}
		response, err := handleRequest(context.Background(), relay, *decoded.Envelope, nil)
		runtimeRelayMust(t, err)
		var written bytes.Buffer
		runtimeRelayMust(t, writeResponse(&written, &response))
		if got := strings.TrimSuffix(written.String(), "\n"); got != want {
			t.Errorf("%s\nGo answered   %s\nRust answered %s", request, got, want)
		}
	}
}

func TestRequestLineLimitIsEnforcedWhileTheLineIsRead(t *testing.T) {
	maximum := 64 * 1024
	exactInput := append(bytes.Repeat([]byte{'1'}, maximum), []byte("\nnext\n")...)
	exact := bufio.NewReader(bytes.NewReader(exactInput))
	line, ok, err := readBoundedLine(exact, maximum)
	runtimeRelayMust(t, err)
	if !ok {
		t.Fatal("the exact line was not read")
	}
	if len(line) != maximum || strings.Trim(line, "1") != "" {
		t.Fatalf("line of %d bytes", len(line))
	}
	line, ok, err = readBoundedLine(exact, maximum)
	runtimeRelayMust(t, err)
	if !ok || line != "next" {
		t.Fatalf("next line = %q, %v", line, ok)
	}

	oversizedInput := bytes.Repeat([]byte{'9'}, maximum+1)
	oversized := bufio.NewReaderSize(bytes.NewReader(oversizedInput), 4)
	_, _, err = readBoundedLine(oversized, maximum)
	if err == nil || !strings.Contains(err.Error(), "request frame is too large") {
		t.Fatalf("%v", err)
	}
}

func TestRepeatedDispatchWakesCoalesceToOnePendingToken(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	wakes := newDispatchWakeChannel()

	for range 10_000 {
		runtimeRelayMust(t, wakeDispatch(relay, wakes))
	}
	select {
	case <-wakes.receive():
	default:
		t.Fatal("no wake is pending")
	}
	select {
	case <-wakes.receive():
		t.Fatal("repeated wakes did not coalesce")
	default:
	}
}

func TestCoordinatorFailureWaitsForPeerCleanupAndPreservesTheCause(t *testing.T) {
	ctx, shutdown := context.WithCancel(context.Background())
	defer shutdown()
	var stopped atomic.Bool
	peer := core.Spawn(func() error {
		<-ctx.Done()
		time.Sleep(25 * time.Millisecond)
		stopped.Store(true)
		return nil
	})
	err := stopPeerAndReturn(peer, shutdown, errors.New("original coordinator failure"), "relay coordinator failed")
	if err == nil {
		t.Fatal("the coordinator failure was dropped")
	}
	if !stopped.Load() {
		t.Fatal("the failure returned before the peer cleaned up")
	}
	select {
	case <-peer.Done():
	default:
		t.Fatal("the peer is still running")
	}
	if !strings.Contains(err.Error(), "original coordinator failure") {
		t.Fatalf("%v", err)
	}
}

func runtimeRelaySessionOpened(t *testing.T, relay *DurableRelay, nativeSessionID string, resumed bool) {
	t.Helper()
	_, err := relay.RecordObservation(core.RelayObservationSessionOpened{
		NativeSessionID:               nativeSessionID,
		Resumed:                       resumed,
		NativeContinuityLost:          false,
		ReplacedUnusedNativeSessionID: nil,
	})
	runtimeRelayMust(t, err)
}

func TestResumeUsesTheLatestRecordedIdentityBeforeTheLaunchIdentity(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelaySessionOpened(t, relay, "native-relay", true)
	config := launchConfig("/var/lib/hel/profiles/session")
	if resume := selectResumeSession(&config, relay); !runtimeRelayIs(resume, "native-relay") {
		t.Fatalf("resume = %v", resume)
	}
	config.NativeSessionID = runtimeRelayString("native-explicit")
	if resume := selectResumeSession(&config, relay); !runtimeRelayIs(resume, "native-relay") {
		t.Fatalf("resume = %v", resume)
	}
}

func TestAQueuedPromptKeepsTheUnusedCodexThreadEligibleForReplacement(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelaySessionOpened(t, relay, "unused", false)
	runtimeRelayTestsSubmit(t, relay, "first-prompt", runtimeRelayTestsPrompt("keep this queued prompt"))
	config := launchConfig("/profile")
	config.NativeSessionID = runtimeRelayString("unused")
	// ACP still needs the original identity so it can name the thread it replaces.
	if resume := selectResumeSession(&config, relay); !runtimeRelayIs(resume, "unused") {
		t.Fatalf("resume = %v", resume)
	}
	if got := relay.NativeSessionHistory(); got != NativeSessionHistoryCreatedHereUnused {
		t.Fatalf("native session history = %v", got)
	}
	if active := relay.OperationalState().ActivePrompt; active == nil || active.CommandID != "first-prompt" {
		t.Fatalf("active prompt = %#v", active)
	}
}

func TestAnImportedNativeIdentityIsRecordedAsUsedAtStartup(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayOpen(t, temp, "1.0.0")
	config := launchConfig("/profile")
	config.NativeSessionID = runtimeRelayString("imported")
	runtimeRelayMust(t, recordImportedNativeIdentity(&config, relay))
	if !relay.NativeSessionMayHaveHistory() {
		t.Fatal("an imported identity may have history")
	}
	if resume := selectResumeSession(&config, relay); !runtimeRelayIs(resume, "imported") {
		t.Fatalf("resume = %v", resume)
	}

	// A thread this journal opened itself is not imported, even when the
	// launch configuration still names it.
	local := runtimeRelayOpen(t, t.TempDir(), "1.0.0")
	runtimeRelaySessionOpened(t, local, "imported", false)
	runtimeRelayMust(t, recordImportedNativeIdentity(&config, local))
	if local.NativeSessionMayHaveHistory() {
		t.Fatal("a thread this journal opened was counted as imported")
	}
}

func TestAUsedNativeSessionIsReportedAsUsedAfterAWorkerRestart(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelaySessionOpened(t, relay, "used", false)
	// What the ACP layer reports when the agent sends content or a prompt is
	// transmitted.
	runtimeRelayRecord(t, NewRelayCell(relay), map[string]core.RelayCommand{}, core.RuntimeEventNativeSessionUsed{})
	reopened := runtimeRelayOpen(t, temp, "1.0.0")
	if !reopened.NativeSessionMayHaveHistory() {
		t.Fatal("a used native session was forgotten across a restart")
	}
}

// pagedRelayHistory is a history that seals several journal segments and
// overflows one replay page, so an attach against it really reads and
// decompresses from disk.
func pagedRelayHistory(t *testing.T, root string, events int) *DurableRelay {
	t.Helper()
	relay := runtimeRelayOpen(t, root, "1.0.0")
	for index := range events {
		_, err := relay.RecordObservation(core.RelayObservationWarning{
			Message: fmt.Sprintf("%04d:%s", index, strings.Repeat("x", 64*1024)),
		})
		runtimeRelayMust(t, err)
	}
	return relay
}

func attachFrame(requestID string, afterOrdinal uint64, afterDigest string) []byte {
	encoded, err := json.Marshal(core.RelayRequestEnvelope{
		RequestID:       requestID,
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         core.RelayRequestAttach{AfterOrdinal: afterOrdinal, AfterDigest: afterDigest, KnownStateDigest: nil},
	})
	if err != nil {
		panic(err)
	}
	return append(encoded, '\n')
}

// LiveRecorder records small observations through the relay lock until it
// is stopped, reporting how many landed and the worst time it ever waited
// for the lock. The wait is the contention this test is about; the append's
// own fsync is deliberately left out of it.
type LiveRecorder struct {
	stopping *atomic.Bool
	task     *core.JoinHandle[liveRecording]
}

// liveRecording is what a stopped LiveRecorder reports: Rust's
// (u64, Duration) task result.
type liveRecording struct {
	recorded  uint64
	worstWait time.Duration
}

// liveRecorderStart starts recording into relay (Rust's LiveRecorder::start).
func liveRecorderStart(relay *RelayCell) *LiveRecorder {
	// Sample the lock often enough that a page read cannot hide in the
	// gaps, but append rarely enough that the fsync per event neither
	// dominates the sampling nor grows the active segment far enough
	// to seal it under the reader.
	const samplesPerRecord = 8
	const recordLimit = 512
	stopping := &atomic.Bool{}
	task := core.SpawnBlocking(func() liveRecording {
		samples := uint64(0)
		recorded := uint64(0)
		worstWait := time.Duration(0)
		for !stopping.Load() {
			waiting := time.Now()
			guard := relay.Lock()
			worstWait = max(worstWait, time.Since(waiting))
			if samples%samplesPerRecord == 0 && recorded < recordLimit {
				if _, err := guard.RecordObservation(core.RelayObservationWarning{
					Message: fmt.Sprintf("live-%d", recorded),
				}); err != nil {
					guard.Unlock()
					panic(err)
				}
				recorded++
			}
			guard.Unlock()
			samples++
			time.Sleep(200 * time.Microsecond)
		}
		return liveRecording{recorded: recorded, worstWait: worstWait}
	})
	return &LiveRecorder{stopping: stopping, task: task}
}

func (r *LiveRecorder) stop(t *testing.T) (uint64, time.Duration) {
	t.Helper()
	r.stopping.Store(true)
	recording, err := r.task.Join(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	return recording.recorded, recording.worstWait
}

// Controllers are supposed to come and go without perturbing the session.
// A catch-up over a long offline history reads page after page from disk
// and decompresses sealed segments; doing that under the relay lock stops
// the coordinator from recording ACP events, and once its bounded channel
// fills the agent's turn stops with it.
func TestAReplayingControllerDoesNotStallLiveEventRecording(t *testing.T) {
	temp := t.TempDir()
	durable := pagedRelayHistory(t, temp, 80)
	frontier := durable.LatestOrdinal()
	relay := NewRelayCell(durable)

	client, served := runtimeRelayServeClient(t, relay)

	recorder := liveRecorderStart(relay)
	runtimeRelayWrite(t, client, attachFrame("catch-up", 0, core.RelayEventGenesisDigest))
	line := runtimeRelayReadLine(t, bufio.NewReader(client))
	recorded, worstWait := recorder.stop(t)

	response := runtimeRelayDecode(t, line)
	attached, ok := runtimeRelayOk(t, response, "catch-up attach failed").(core.RelayResponsePayloadAttached)
	if !ok {
		t.Fatalf("catch-up attach failed: %#v", response.Body)
	}
	if len(attached.Events) == 0 || attached.ThroughOrdinal >= frontier {
		t.Fatalf("this history should need several pages: %d of %d", attached.ThroughOrdinal, frontier)
	}

	// What one page costs to assemble, measured on this machine with
	// nothing else running. Serving it under the relay lock would make a
	// recorder wait about this long; serving it off the lock costs a
	// recorder only the plan capture.
	pageStarted := time.Now()
	_, err := handleRequest(context.Background(), relay, core.RelayRequestEnvelope{
		RequestID:       "page-cost",
		ProtocolVersion: core.RelayProtocolVersion,
		Request: core.RelayRequestAttach{
			AfterOrdinal:     0,
			AfterDigest:      core.RelayEventGenesisDigest,
			KnownStateDigest: nil,
		},
	}, nil)
	runtimeRelayMust(t, err)
	pageCost := time.Since(pageStarted)
	if pageCost < 10*time.Millisecond {
		t.Fatalf("a page assembled in %v; too cheap to say anything about contention", pageCost)
	}
	if recorded == 0 {
		t.Fatal("no event was recorded during the replay")
	}
	if worstWait*3 >= pageCost {
		t.Fatalf("recording waited %v for a page that takes %v to assemble: the page is being read under the relay lock", worstWait, pageCost)
	}

	client.Close()
	runtimeRelayJoinOk(t, served, "serve the catch-up connection")
}

// Sealing the active segment moves events into a file no captured replay
// plan names, and a busy session seals one per megabyte of transcript. A
// controller catching up through that must still get its pages: the relay
// plans again against the journal as it now stands instead of handing the
// controller a failure it did nothing to cause.
func TestACatchUpCompletesWhileTheJournalKeepsSealingUnderIt(t *testing.T) {
	temp := t.TempDir()
	durable := pagedRelayHistory(t, temp, 96)
	target := durable.LatestOrdinal()
	generation := durable.JournalGeneration()
	relay := NewRelayCell(durable)

	// Events large enough to seal a segment every few appends, written as
	// fast as the durable path allows.
	var stop atomic.Bool
	writer := core.SpawnBlocking(func() uint64 {
		written := uint64(0)
		for !stop.Load() && written < 24 {
			guard := relay.Lock()
			_, err := guard.RecordObservation(core.RelayObservationWarning{
				Message: fmt.Sprintf("%04d:%s", written, strings.Repeat("y", 256*1024)),
			})
			guard.Unlock()
			if err != nil {
				panic(err)
			}
			written++
		}
		return written
	})

	cursorOrdinal, cursorDigest := uint64(0), core.RelayEventGenesisDigest
	pages := 0
	for cursorOrdinal < target {
		response, err := handleRequest(context.Background(), relay, core.RelayRequestEnvelope{
			RequestID:       fmt.Sprintf("sealing-page-%d", pages),
			ProtocolVersion: core.RelayProtocolVersion,
			Request: core.RelayRequestAttach{
				AfterOrdinal:     cursorOrdinal,
				AfterDigest:      cursorDigest,
				KnownStateDigest: nil,
			},
		}, nil)
		runtimeRelayMust(t, err)
		body, _ := response.Body.(core.RelayResponseBodyOk)
		attached, ok := body.Payload.(core.RelayResponsePayloadAttached)
		if !ok {
			stop.Store(true)
			t.Fatalf("page %d of a catch-up under a sealing journal failed: %#v", pages, response.Body)
		}
		if attached.ThroughOrdinal <= cursorOrdinal {
			stop.Store(true)
			t.Fatalf("page %d made no progress from event %d", pages, cursorOrdinal)
		}
		cursorOrdinal, cursorDigest = attached.ThroughOrdinal, attached.ThroughDigest
		pages++
	}
	stop.Store(true)
	runtimeRelayJoin(t, writer, "sealing writer")

	locked := relay.Lock()
	sealed := locked.JournalGeneration() > generation
	locked.Unlock()
	if !sealed {
		t.Fatal("the journal never sealed, so this run proved nothing")
	}
}

// Recording throughput during a full catch-up, measured with the page read
// inside and outside the relay lock so both policies are timed in one
// process. Rust ignores it by default; run it with MJ_RUN_IGNORED=1 and
// `go test -run TestCatchUpRecordingThroughput -v`.
func TestCatchUpRecordingThroughput(t *testing.T) {
	if os.Getenv("MJ_RUN_IGNORED") == "" {
		t.Skip("timing measurement, not a behavior assertion")
	}
	const historyEvents = 200

	catchUp := func(deferPageReads bool) (uint64, time.Duration, int) {
		temp := t.TempDir()
		durable := pagedRelayHistory(t, temp, historyEvents)
		frontier := durable.LatestOrdinal()
		relay := NewRelayCell(durable)
		recorder := liveRecorderStart(relay)

		started := time.Now()
		cursorOrdinal, cursorDigest := uint64(0), core.RelayEventGenesisDigest
		pages := 0
		for cursorOrdinal < frontier {
			envelope := core.RelayRequestEnvelope{
				RequestID:       fmt.Sprintf("page-%d", pages),
				ProtocolVersion: core.RelayProtocolVersion,
				Request: core.RelayRequestAttach{
					AfterOrdinal:     cursorOrdinal,
					AfterDigest:      cursorDigest,
					KnownStateDigest: nil,
				},
			}
			var response core.RelayResponseEnvelope
			if deferPageReads {
				var err error
				response, err = handleRequest(context.Background(), relay, envelope, nil)
				runtimeRelayMust(t, err)
			} else {
				// The coupled policy: the whole read runs under the lock.
				guard := relay.Lock()
				response = guard.Handle(envelope)
				guard.Unlock()
			}
			body, _ := response.Body.(core.RelayResponseBodyOk)
			attached, ok := body.Payload.(core.RelayResponsePayloadAttached)
			if !ok {
				t.Fatalf("catch-up page %d (defer=%v) failed: %#v", pages, deferPageReads, response.Body)
			}
			cursorOrdinal, cursorDigest = attached.ThroughOrdinal, attached.ThroughDigest
			pages++
		}
		elapsed := time.Since(started)
		recorded, _ := recorder.stop(t)
		return recorded, elapsed, pages
	}

	for round := range 2 {
		coupled, coupledElapsed, pages := catchUp(false)
		deferred, deferredElapsed, _ := catchUp(true)
		t.Logf("round %d: %d pages | under the lock %d events in %v | off the lock %d events in %v",
			round, pages, coupled, coupledElapsed, deferred, deferredElapsed)
	}
}

// runtimeRelayMemoryConfig is a project-memory launch configuration whose
// replica and baseline live under directory.
func runtimeRelayMemoryConfig(directory string) core.ProjectMemoryLaunchConfig {
	return core.ProjectMemoryLaunchConfig{
		HistorySocket:   nil,
		ProjectKey:      "project",
		Root:            filepath.Join(directory, "replica"),
		BaselineRoot:    filepath.Join(directory, "baseline"),
		RepositoryRoots: map[string]string{},
		MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
	}
}

// runtimeRelayInstall installs files as a store's whole tree.
func runtimeRelayInstall(t *testing.T, store *core.ProjectMemoryStore, files map[string]string) core.ProjectMemorySnapshot {
	t.Helper()
	snapshot := core.ProjectMemorySnapshot{Files: files}
	_, err := store.InstallSnapshot(&snapshot)
	runtimeRelayMust(t, err)
	return snapshot
}

// runtimeRelaySnapshot is a store's current tree.
func runtimeRelaySnapshot(t *testing.T, store *core.ProjectMemoryStore) core.ProjectMemorySnapshot {
	t.Helper()
	snapshot, err := store.Snapshot()
	runtimeRelayMust(t, err)
	return snapshot
}

// runtimeRelaySameTree requires two trees to hold the same files.
func runtimeRelaySameTree(t *testing.T, got, want core.ProjectMemorySnapshot, what string) {
	t.Helper()
	if !maps.Equal(got.Files, want.Files) {
		t.Fatalf("%s holds %d files, want %d: %v", what, len(got.Files), len(want.Files), slices.Sorted(maps.Keys(got.Files)))
	}
}

func TestRelativePathsAreResolvedBeforeTheBridgeChangesDirectory(t *testing.T) {
	config := launchConfig(".local/share/hel/profiles/session")
	relativeMemory := ".local/share/hel/profiles/session/projects/project/memory"
	config.AdditionalDirectories = append(config.AdditionalDirectories, relativeMemory)
	config.ProjectMemory = &core.ProjectMemoryLaunchConfig{
		HistorySocket:   nil,
		ProjectKey:      "project",
		Root:            relativeMemory,
		BaselineRoot:    "",
		RepositoryRoots: map[string]string{},
		MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
	}
	resolveRelativeHarnessHome(&config, "/home/ubuntu")
	if got := config.Environment["CODEX_HOME"]; got != "/home/ubuntu/.local/share/hel/profiles/session" {
		t.Fatalf("CODEX_HOME = %q", got)
	}
	if want := []string{"/home/ubuntu/.local/share/hel/profiles/session/projects/project/memory"}; !slices.Equal(config.AdditionalDirectories, want) {
		t.Fatalf("additional directories = %q", config.AdditionalDirectories)
	}
	if got := resolveRelativeWorkerRoot(".local/share/hel/workers/session", "/home/ubuntu"); got != "/home/ubuntu/.local/share/hel/workers/session" {
		t.Fatalf("worker root = %q", got)
	}
}

func TestReviewerWorkspaceRootsExcludeThePrimaryProjectMemoryReplica(t *testing.T) {
	workspace := "/workspace/api"
	memoryRoot := "/profile/projects/session/memory"
	config := launchConfig("/profile")
	config.AdditionalDirectories = []string{workspace, memoryRoot}
	config.ProjectMemory = &core.ProjectMemoryLaunchConfig{
		HistorySocket:   nil,
		ProjectKey:      "project",
		Root:            memoryRoot,
		BaselineRoot:    "",
		RepositoryRoots: map[string]string{},
		MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
	}

	if got := reviewerWorkspaceDirectories(&config); !slices.Equal(got, []string{workspace}) {
		t.Fatalf("reviewer roots = %q", got)
	}
}

func TestAcpWorkspaceRootsAddTheMemoryReplicaOutsideControllerAttachments(t *testing.T) {
	workspace := "/workspace/api"
	memoryRoot := "/profile/projects/session/memory"

	for _, harness := range []core.HarnessKind{core.HarnessKindCodex, core.HarnessKindKimi, core.HarnessKindGrok} {
		config := launchConfig("/profile")
		config.Harness = harness
		config.AdditionalDirectories = []string{workspace}
		config.ProjectMemory = &core.ProjectMemoryLaunchConfig{
			HistorySocket:   nil,
			ProjectKey:      "project",
			Root:            memoryRoot,
			BaselineRoot:    "",
			RepositoryRoots: map[string]string{},
			MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
		}

		if got := acpAdditionalDirectories(&config); !slices.Equal(got, []string{workspace, memoryRoot}) {
			t.Fatalf("%v receives its replica only in the ACP workspace roots: %q", harness, got)
		}
	}

	for _, harness := range []core.HarnessKind{core.HarnessKindClaude, core.HarnessKindMuse} {
		config := launchConfig("/profile")
		config.Harness = harness
		config.ProjectMemory = &core.ProjectMemoryLaunchConfig{
			HistorySocket:   nil,
			ProjectKey:      "project",
			Root:            memoryRoot,
			BaselineRoot:    "",
			RepositoryRoots: map[string]string{},
			MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
		}
		if got := acpAdditionalDirectories(&config); len(got) != 0 {
			t.Fatalf("%v: ACP roots = %q", harness, got)
		}
	}
}

// Hard-won: #1305: one file over 100 KiB blocked every memory snapshot and checkpoint.
func TestProjectMemoryConnectionRequestsRoundTripReplicaAndBaseline(t *testing.T) {
	directory := t.TempDir()
	largeMemory := strings.Repeat("hourly CI check\n", 220_000)
	memory := runtimeRelayMemoryConfig(directory)
	baseline := core.NewProjectMemoryStore(memory.BaselineRoot)
	runtimeRelayInstall(t, &baseline, map[string]string{
		"/MEMORY.md":        "base",
		"/baseline-only.md": "keep baseline",
	})
	replica := core.NewProjectMemoryStore(memory.Root)
	runtimeRelayInstall(t, &replica, map[string]string{
		"/MEMORY.md":       largeMemory,
		"/replica-only.md": "keep replica",
	})

	payload, err := applyProjectMemoryRequest(&memory, core.RelayRequestProjectMemorySnapshot{})
	runtimeRelayMust(t, err)
	snapshot, ok := payload.(core.RelayResponsePayloadProjectMemorySnapshot)
	if !ok {
		t.Fatalf("unexpected project-memory payload %#v", payload)
	}
	if _, ok := snapshot.Baseline.(core.ProjectMemoryWireTreeGzip); !ok {
		t.Fatalf("baseline = %T", snapshot.Baseline)
	}
	if _, ok := snapshot.Replica.(core.ProjectMemoryWireTreeGzip); !ok {
		t.Fatalf("replica = %T", snapshot.Replica)
	}
	capturedBaseline, err := core.ProjectMemoryWireTreeDecode(snapshot.Baseline)
	runtimeRelayMust(t, err)
	if capturedBaseline.Files["/MEMORY.md"] != "base" {
		t.Fatalf("baseline MEMORY.md = %q", capturedBaseline.Files["/MEMORY.md"])
	}
	capturedReplica, err := core.ProjectMemoryWireTreeDecode(snapshot.Replica)
	runtimeRelayMust(t, err)
	if capturedReplica.Files["/MEMORY.md"] != largeMemory {
		t.Fatal("the replica's large MEMORY.md did not round-trip")
	}

	_, err = applyProjectMemoryRequest(&memory, core.RelayRequestInstallProjectMemorySnapshot{
		Snapshot: core.ProjectMemoryWireTreePlain{Snapshot: core.ProjectMemorySnapshot{
			Files: map[string]string{"/MEMORY.md": largeMemory},
		}},
	})
	runtimeRelayMust(t, err)
	installedBaseline := runtimeRelaySnapshot(t, &baseline)
	installedReplica := runtimeRelaySnapshot(t, &replica)
	if installedBaseline.Files["/MEMORY.md"] != largeMemory {
		t.Fatal("the baseline did not install the large MEMORY.md")
	}
	if installedBaseline.Files["/baseline-only.md"] != "keep baseline" {
		t.Fatalf("baseline-only.md = %q", installedBaseline.Files["/baseline-only.md"])
	}
	if installedReplica.Files["/MEMORY.md"] != largeMemory {
		t.Fatal("the replica did not install the large MEMORY.md")
	}
	if installedReplica.Files["/replica-only.md"] != "keep replica" {
		t.Fatalf("replica-only.md = %q", installedReplica.Files["/replica-only.md"])
	}
}

// Hard-won: #1305: the document cap also prevented installing large merged files.
func TestProjectMemoryReplaceDeletesFilesOmittedFromTreeInReplicaAndBaseline(t *testing.T) {
	directory := t.TempDir()
	memory := runtimeRelayMemoryConfig(directory)
	baseline := core.NewProjectMemoryStore(memory.BaselineRoot)
	runtimeRelayInstall(t, &baseline, map[string]string{
		"/MEMORY.md":       "baseline memory",
		"/old-baseline.md": "remove",
	})
	replica := core.NewProjectMemoryStore(memory.Root)
	runtimeRelayInstall(t, &replica, map[string]string{
		"/MEMORY.md":      "replica memory",
		"/old-replica.md": "remove",
	})
	current := runtimeRelaySnapshot(t, &replica)
	expectedReplica := current.Version()
	tree := core.ProjectMemorySnapshot{Files: map[string]string{"/MEMORY.md": strings.Repeat("reconciled\n", 320_000)}}

	wire, err := core.ProjectMemoryWireTreeEncode(tree, core.RelayProtocolVersion)
	runtimeRelayMust(t, err)
	payload, err := applyProjectMemoryRequest(&memory, core.RelayRequestReplaceProjectMemoryTree{
		ExpectedReplica: expectedReplica,
		Tree:            wire,
	})
	runtimeRelayMust(t, err)

	if replaced, ok := payload.(core.RelayResponsePayloadProjectMemoryTreeReplaced); !ok || replaced.Outcome != core.ReplicaReplaceOutcomeReplaced {
		t.Fatalf("payload = %#v", payload)
	}
	runtimeRelaySameTree(t, runtimeRelaySnapshot(t, &replica), tree, "replica")
	runtimeRelaySameTree(t, runtimeRelaySnapshot(t, &baseline), tree, "baseline")
}

func TestProjectMemoryReplaceRefusesAChangedReplicaWithoutWritingEitherTree(t *testing.T) {
	directory := t.TempDir()
	memory := runtimeRelayMemoryConfig(directory)
	baseline := core.NewProjectMemoryStore(memory.BaselineRoot)
	runtimeRelayInstall(t, &baseline, map[string]string{"/MEMORY.md": "baseline"})
	replica := core.NewProjectMemoryStore(memory.Root)
	runtimeRelayInstall(t, &replica, map[string]string{"/MEMORY.md": "initial replica"})
	initial := runtimeRelaySnapshot(t, &replica)
	expectedReplica := initial.Version()
	baselineBefore := runtimeRelaySnapshot(t, &baseline)
	runtimeRelayInstall(t, &replica, map[string]string{"/concurrent.md": "external edit"})
	replicaAfterConcurrentEdit := runtimeRelaySnapshot(t, &replica)
	tree := core.ProjectMemorySnapshot{Files: map[string]string{"/MEMORY.md": "reconciled"}}

	payload, err := applyProjectMemoryRequest(&memory, core.RelayRequestReplaceProjectMemoryTree{
		ExpectedReplica: expectedReplica,
		Tree:            core.ProjectMemoryWireTreePlain{Snapshot: tree},
	})
	runtimeRelayMust(t, err)

	if replaced, ok := payload.(core.RelayResponsePayloadProjectMemoryTreeReplaced); !ok || replaced.Outcome != core.ReplicaReplaceOutcomeReplicaChanged {
		t.Fatalf("payload = %#v", payload)
	}
	runtimeRelaySameTree(t, runtimeRelaySnapshot(t, &replica), replicaAfterConcurrentEdit, "replica")
	runtimeRelaySameTree(t, runtimeRelaySnapshot(t, &baseline), baselineBefore, "baseline")
}

// Hard-won: #1500: a complete project-memory tree exceeded the single relay frame limit.
func TestProjectMemoryTransferPagesLargeTreesAndCommitsCompleteUploads(t *testing.T) {
	directory := t.TempDir()
	largeBaseline := incompressibleMemoryText(10 * 1024 * 1024)
	largeReplica := incompressibleMemoryText(10 * 1024 * 1024)
	largeUpload := incompressibleMemoryText(10 * 1024 * 1024)
	memory := runtimeRelayMemoryConfig(directory)
	baselineStore := core.NewProjectMemoryStore(memory.BaselineRoot)
	baselineTree := runtimeRelayInstall(t, &baselineStore, map[string]string{
		"/baseline-only.md": "baseline",
		"/large.md":         largeBaseline,
	})
	replicaStore := core.NewProjectMemoryStore(memory.Root)
	replicaTree := runtimeRelayInstall(t, &replicaStore, map[string]string{
		"/large.md":        largeReplica,
		"/replica-only.md": "replica",
	})
	encodedSnapshot, err := core.ProjectMemoryWireSnapshotEncode(baselineTree, replicaTree, core.RelayProtocolVersion)
	runtimeRelayMust(t, err)
	if len(encodedSnapshot) <= core.MaxFrameBytes {
		t.Fatalf("the snapshot must exceed one frame, got %d bytes", len(encodedSnapshot))
	}

	uploadTree := core.ProjectMemorySnapshot{Files: map[string]string{"/large-upload.md": largeUpload}}
	uploadWire, err := core.ProjectMemoryWireTreeEncode(uploadTree, core.RelayProtocolVersion)
	runtimeRelayMust(t, err)
	encodedUpload := runtimeRelayJSON(t, uploadWire)
	if len(encodedUpload) <= core.MaxFrameBytes {
		t.Fatalf("the upload must exceed one frame, got %d bytes", len(encodedUpload))
	}

	relay := runtimeRelayCell(t, filepath.Join(directory, "relay"), "test")
	endpoint := newProjectMemoryEndpoint(&memory)
	client, serverTask := projectMemorySocket(t, relay, endpoint)
	lines := bufio.NewReader(client)
	sequence := uint64(0)

	response := nextProjectMemoryTransferExchange(t, lines, client, &sequence, core.ProjectMemoryTransferActionBeginSnapshot{})
	started, ok := runtimeRelayTransferResult(response).(core.ProjectMemoryTransferResultSnapshotStarted)
	if !ok {
		t.Fatalf("project-memory snapshot did not start: %#v", response.Body)
	}
	transferID, totalBytes := started.TransferID, started.TotalBytes
	if int(totalBytes) != len(encodedSnapshot) {
		t.Fatalf("total bytes = %d, want %d", totalBytes, len(encodedSnapshot))
	}

	// A second socket cannot consume another connection's immutable transfer.
	otherClient, otherTask := projectMemorySocket(t, relay, endpoint)
	otherLines := bufio.NewReader(otherClient)
	otherResponse := projectMemoryTransferExchange(t, otherLines, otherClient, "other-connection", core.ProjectMemoryTransferActionReadSnapshotPage{
		TransferID: transferID,
		Offset:     0,
	})
	if _, ok := otherResponse.Body.(core.RelayResponseBodyError); !ok {
		t.Fatalf("another connection read the transfer: %#v", otherResponse.Body)
	}
	otherClient.Close()
	runtimeRelayJoinOk(t, otherTask, "serve the other connection")

	// Mutation after BeginSnapshot cannot change any later page in the frozen pair.
	runtimeRelayInstall(t, &baselineStore, map[string]string{"/changed-after-capture.md": "baseline"})
	runtimeRelayInstall(t, &replicaStore, map[string]string{"/changed-after-capture.md": "replica"})
	receivedSnapshot := []byte{}
	offset := uint64(0)
	firstPage := true
	for {
		action := core.ProjectMemoryTransferActionReadSnapshotPage{TransferID: transferID, Offset: offset}
		var response core.RelayResponseEnvelope
		if firstPage {
			firstPage = false
			escapedLongID := "long-" + strings.Repeat("\"", 220)
			response = projectMemoryTransferExchange(t, lines, client, escapedLongID, action)
		} else {
			response = nextProjectMemoryTransferExchange(t, lines, client, &sequence, action)
		}
		page, ok := runtimeRelayTransferResult(response).(core.ProjectMemoryTransferResultSnapshotPage)
		if !ok {
			t.Fatalf("project-memory snapshot page failed: %#v", response.Body)
		}
		if page.TransferID != transferID || page.Offset != offset {
			t.Fatalf("page names transfer %d at %d, want %d at %d", page.TransferID, page.Offset, transferID, offset)
		}
		chunk, err := base64.StdEncoding.DecodeString(page.Chunk)
		runtimeRelayMust(t, err)
		receivedSnapshot = append(receivedSnapshot, chunk...)
		if page.NextOffset <= offset {
			t.Fatalf("page at %d made no progress", offset)
		}
		offset = page.NextOffset
		if page.Complete {
			break
		}
	}
	if offset != totalBytes {
		t.Fatalf("read %d of %d bytes", offset, totalBytes)
	}
	if !bytes.Equal(receivedSnapshot, encodedSnapshot) {
		t.Fatal("the paged snapshot differs from the captured pair")
	}
	decodedBaseline, decodedReplica, err := core.ProjectMemoryWireSnapshotDecode(receivedSnapshot)
	runtimeRelayMust(t, err)
	runtimeRelaySameTree(t, decodedBaseline, baselineTree, "received baseline")
	runtimeRelaySameTree(t, decodedReplica, replicaTree, "received replica")

	installed := uploadProjectMemoryBytes(t, lines, client, &sequence, encodedUpload, core.ProjectMemoryUploadKindInstallSnapshot{})
	if transfer, ok := installed.(core.RelayResponsePayloadProjectMemoryTransfer); !ok {
		t.Fatalf("installed = %#v", installed)
	} else if _, ok := transfer.Result.(core.ProjectMemoryTransferResultInstalled); !ok {
		t.Fatalf("installed = %#v", installed)
	}
	if runtimeRelaySnapshot(t, &baselineStore).Files["/large-upload.md"] != uploadTree.Files["/large-upload.md"] {
		t.Fatal("the baseline did not install the upload")
	}
	if runtimeRelaySnapshot(t, &replicaStore).Files["/large-upload.md"] != uploadTree.Files["/large-upload.md"] {
		t.Fatal("the replica did not install the upload")
	}
	if _, ok := runtimeRelaySnapshot(t, &baselineStore).Files["/baseline-only.md"]; !ok {
		t.Fatal("an installed snapshot removed baseline-only.md")
	}
	if _, ok := runtimeRelaySnapshot(t, &replicaStore).Files["/replica-only.md"]; !ok {
		t.Fatal("an installed snapshot removed replica-only.md")
	}

	currentReplica := runtimeRelaySnapshot(t, &replicaStore)
	expectedReplica := currentReplica.Version()
	replaced := uploadProjectMemoryBytes(t, lines, client, &sequence, encodedUpload, core.ProjectMemoryUploadKindReplaceTree{
		ExpectedReplica: expectedReplica,
	})
	if transfer, ok := replaced.(core.RelayResponsePayloadProjectMemoryTransfer); !ok {
		t.Fatalf("replaced = %#v", replaced)
	} else if result, ok := transfer.Result.(core.ProjectMemoryTransferResultReplaced); !ok || result.Outcome != core.ReplicaReplaceOutcomeReplaced {
		t.Fatalf("replaced = %#v", replaced)
	}
	runtimeRelaySameTree(t, runtimeRelaySnapshot(t, &baselineStore), uploadTree, "baseline")
	runtimeRelaySameTree(t, runtimeRelaySnapshot(t, &replicaStore), uploadTree, "replica")

	staleReplica := runtimeRelaySnapshot(t, &replicaStore)
	staleExpected := staleReplica.Version()
	staleTree := core.ProjectMemorySnapshot{Files: map[string]string{"/must-not-appear.md": "stale upload"}}
	staleWire, err := core.ProjectMemoryWireTreeEncode(staleTree, core.RelayProtocolVersion)
	runtimeRelayMust(t, err)
	staleBytes := runtimeRelayJSON(t, staleWire)
	response = nextProjectMemoryTransferExchange(t, lines, client, &sequence, core.ProjectMemoryTransferActionBeginUpload{
		Kind:       core.ProjectMemoryUploadKindReplaceTree{ExpectedReplica: staleExpected},
		TotalBytes: uint64(len(staleBytes)),
	})
	staleStarted, ok := runtimeRelayTransferResult(response).(core.ProjectMemoryTransferResultUploadStarted)
	if !ok {
		t.Fatalf("stale project-memory upload did not start: %#v", response.Body)
	}
	staleID := staleStarted.TransferID
	response = nextProjectMemoryTransferExchange(t, lines, client, &sequence, core.ProjectMemoryTransferActionAppendUpload{
		TransferID: staleID,
		Offset:     0,
		Chunk:      base64.StdEncoding.EncodeToString(staleBytes),
	})
	runtimeRelayOk(t, response, "append the stale upload")
	runtimeRelayInstall(t, &replicaStore, map[string]string{"/external.md": "concurrent edit"})
	baselineBeforeStaleCommit := runtimeRelaySnapshot(t, &baselineStore)
	replicaBeforeStaleCommit := runtimeRelaySnapshot(t, &replicaStore)
	staleResult := nextProjectMemoryTransferExchange(t, lines, client, &sequence, core.ProjectMemoryTransferActionCommitUpload{TransferID: staleID})
	if result, ok := runtimeRelayTransferResult(staleResult).(core.ProjectMemoryTransferResultReplaced); !ok || result.Outcome != core.ReplicaReplaceOutcomeReplicaChanged {
		t.Fatalf("stale commit = %#v", staleResult.Body)
	}
	runtimeRelaySameTree(t, runtimeRelaySnapshot(t, &baselineStore), baselineBeforeStaleCommit, "baseline")
	runtimeRelaySameTree(t, runtimeRelaySnapshot(t, &replicaStore), replicaBeforeStaleCommit, "replica")

	beforeAbort := runtimeRelaySnapshot(t, &replicaStore)
	response = nextProjectMemoryTransferExchange(t, lines, client, &sequence, core.ProjectMemoryTransferActionBeginUpload{
		Kind:       core.ProjectMemoryUploadKindInstallSnapshot{},
		TotalBytes: 100,
	})
	abortStarted, ok := runtimeRelayTransferResult(response).(core.ProjectMemoryTransferResultUploadStarted)
	if !ok {
		t.Fatalf("abort-test upload did not start: %#v", response.Body)
	}
	abortID := abortStarted.TransferID
	response = nextProjectMemoryTransferExchange(t, lines, client, &sequence, core.ProjectMemoryTransferActionAppendUpload{
		TransferID: abortID,
		Offset:     0,
		Chunk:      base64.StdEncoding.EncodeToString([]byte("partial")),
	})
	runtimeRelayOk(t, response, "append the partial upload")
	response = nextProjectMemoryTransferExchange(t, lines, client, &sequence, core.ProjectMemoryTransferActionAbort{TransferID: abortID})
	if aborted, ok := runtimeRelayTransferResult(response).(core.ProjectMemoryTransferResultAborted); !ok || aborted.TransferID != abortID {
		t.Fatalf("abort = %#v", response.Body)
	}
	runtimeRelaySameTree(t, runtimeRelaySnapshot(t, &replicaStore), beforeAbort, "replica")
	client.Close()
	runtimeRelayJoinOk(t, serverTask, "serve the project-memory connection")
}

// Legacy project-memory method shapes remain available at the current protocol.
func TestProtocol59WorkerServesLegacyProjectMemoryMethodsAndGatesTransfer(t *testing.T) {
	directory := t.TempDir()
	memory := runtimeRelayMemoryConfig(directory)
	baseline := core.NewProjectMemoryStore(memory.BaselineRoot)
	runtimeRelayInstall(t, &baseline, map[string]string{"/MEMORY.md": "legacy baseline"})
	replica := core.NewProjectMemoryStore(memory.Root)
	runtimeRelayInstall(t, &replica, map[string]string{"/MEMORY.md": "legacy replica"})
	relay := runtimeRelayCell(t, filepath.Join(directory, "relay"), "test")
	endpoint := newProjectMemoryEndpoint(&memory)
	client, serverTask := projectMemorySocket(t, relay, endpoint)
	lines := bufio.NewReader(client)

	response := runtimeRelayExchange(t, lines, client, core.RelayRequestEnvelope{
		RequestID:       "legacy-memory-snapshot",
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         core.RelayRequestProjectMemorySnapshot{},
	})
	if response.ProtocolVersion != core.RelayProtocolVersion {
		t.Fatalf("protocol version = %d", response.ProtocolVersion)
	}
	if _, ok := runtimeRelayOk(t, response, "legacy snapshot").(core.RelayResponsePayloadProjectMemorySnapshot); !ok {
		t.Fatalf("%#v", response.Body)
	}

	unsupported := projectMemoryTransferExchangeAt(t, lines, client, "transfer-at-protocol-58", core.RelayChangeWaitProtocol, core.ProjectMemoryTransferActionBeginSnapshot{})
	if failure := runtimeRelayError(t, unsupported, "transfer below its protocol"); failure.Code != core.RelayErrorCodeIncompatibleProtocol {
		t.Fatalf("%#v", unsupported.Body)
	}
	client.Close()
	runtimeRelayJoinOk(t, serverTask, "serve the project-memory connection")
}

// A disconnected upload must not publish a partial tree or leak transfer state.
func TestProjectMemoryPartialUploadIsDiscardedWithItsConnection(t *testing.T) {
	directory := t.TempDir()
	memory := runtimeRelayMemoryConfig(directory)
	baseline := core.NewProjectMemoryStore(memory.BaselineRoot)
	replica := core.NewProjectMemoryStore(memory.Root)
	initial := runtimeRelayInstall(t, &baseline, map[string]string{"/MEMORY.md": "initial"})
	runtimeRelayInstall(t, &replica, map[string]string{"/MEMORY.md": "initial"})
	relay := runtimeRelayCell(t, filepath.Join(directory, "relay"), "test")
	endpoint := newProjectMemoryEndpoint(&memory)

	client, disconnectedServer := projectMemorySocket(t, relay, endpoint)
	lines := bufio.NewReader(client)
	sequence := uint64(0)
	started := nextProjectMemoryTransferExchange(t, lines, client, &sequence, core.ProjectMemoryTransferActionBeginUpload{
		Kind:       core.ProjectMemoryUploadKindReplaceTree{ExpectedReplica: initial.Version()},
		TotalBytes: 100,
	})
	upload, ok := runtimeRelayTransferResult(started).(core.ProjectMemoryTransferResultUploadStarted)
	if !ok {
		t.Fatalf("partial project-memory upload did not start: %#v", started.Body)
	}
	transferID := upload.TransferID
	incompleteCommit := nextProjectMemoryTransferExchange(t, lines, client, &sequence, core.ProjectMemoryTransferActionCommitUpload{TransferID: transferID})
	if _, ok := incompleteCommit.Body.(core.RelayResponseBodyError); !ok {
		t.Fatalf("an incomplete upload committed: %#v", incompleteCommit.Body)
	}
	wrongOffset := nextProjectMemoryTransferExchange(t, lines, client, &sequence, core.ProjectMemoryTransferActionAppendUpload{
		TransferID: transferID,
		Offset:     1,
		Chunk:      base64.StdEncoding.EncodeToString([]byte("partial tree")),
	})
	if _, ok := wrongOffset.Body.(core.RelayResponseBodyError); !ok {
		t.Fatalf("a misplaced chunk was accepted: %#v", wrongOffset.Body)
	}
	appended := nextProjectMemoryTransferExchange(t, lines, client, &sequence, core.ProjectMemoryTransferActionAppendUpload{
		TransferID: transferID,
		Offset:     0,
		Chunk:      base64.StdEncoding.EncodeToString([]byte("partial tree")),
	})
	runtimeRelayOk(t, appended, "append the partial upload")
	client.Close()
	runtimeRelayJoinOk(t, disconnectedServer, "serve the disconnected upload")
	runtimeRelaySameTree(t, runtimeRelaySnapshot(t, &baseline), initial, "baseline")
	runtimeRelaySameTree(t, runtimeRelaySnapshot(t, &replica), initial, "replica")

	// A new connection cannot finish or abort the former connection's upload.
	client, nextServer := projectMemorySocket(t, relay, endpoint)
	lines = bufio.NewReader(client)
	result := nextProjectMemoryTransferExchange(t, lines, client, &sequence, core.ProjectMemoryTransferActionCommitUpload{TransferID: transferID})
	if _, ok := result.Body.(core.RelayResponseBodyError); !ok {
		t.Fatalf("another connection committed the upload: %#v", result.Body)
	}
	startedSnapshot := nextProjectMemoryTransferExchange(t, lines, client, &sequence, core.ProjectMemoryTransferActionBeginSnapshot{})
	snapshot, ok := runtimeRelayTransferResult(startedSnapshot).(core.ProjectMemoryTransferResultSnapshotStarted)
	if !ok {
		t.Fatalf("new connection could not start a fresh transfer: %#v", startedSnapshot.Body)
	}
	aborted := nextProjectMemoryTransferExchange(t, lines, client, &sequence, core.ProjectMemoryTransferActionAbort{TransferID: snapshot.TransferID})
	runtimeRelayOk(t, aborted, "abort the fresh transfer")
	client.Close()
	runtimeRelayJoinOk(t, nextServer, "serve the next connection")
}

func TestAbandonedProjectMemoryRequestsDoNotOverlapBlockingIo(t *testing.T) {
	gate := make(chan struct{}, 1)
	firstStarted := make(chan struct{}, 1)
	// Rust's std mpsc sender never blocks.
	releaseFirst := make(chan struct{}, 1)
	firstCtx, abortFirst := context.WithCancel(context.Background())
	first := core.Spawn(func() runtimeRelaySerializedIO {
		outcome, admitted := runSerializedProjectMemoryIO(firstCtx, gate, false, func() (struct{}, error) {
			firstStarted <- struct{}{}
			<-releaseFirst
			return struct{}{}, nil
		})
		return runtimeRelaySerializedIO{outcome: outcome, admitted: admitted}
	})
	// Rust's oneshot receive fails once the task that holds the sender ends.
	select {
	case <-firstStarted:
	case <-first.Done():
		result, err := first.Join(context.Background())
		t.Fatalf("the first request ended before it started: %#v, %v", result, err)
	case <-time.After(ioTimeout):
		t.Fatal("timed out waiting for the first request to start")
	}

	// Model a controller dropping its request future after its transport
	// timeout. The blocking operation survives that cancellation.
	abortFirst()
	secondStarted := make(chan struct{}, 1)
	second := core.Spawn(func() runtimeRelaySerializedIO {
		outcome, admitted := runSerializedProjectMemoryIO(context.Background(), gate, false, func() (struct{}, error) {
			secondStarted <- struct{}{}
			return struct{}{}, nil
		})
		return runtimeRelaySerializedIO{outcome: outcome, admitted: admitted}
	})
	select {
	case <-secondStarted:
		t.Fatal("abandoning a request released its in-flight filesystem permit")
	case <-time.After(100 * time.Millisecond):
	}

	releaseFirst <- struct{}{}
	select {
	case <-secondStarted:
	case <-time.After(time.Second):
		t.Fatal("the queued request did not start after prior I/O completed")
	}
	result := runtimeRelayJoin(t, second, "the second request")
	if !result.admitted || result.outcome.joinErr != nil || result.outcome.err != nil {
		t.Fatalf("second request = %#v", result)
	}
	runtimeRelayJoin(t, first, "the abandoned request")
}

func TestLargePhotoTransferIsVerifiedWithoutJournalingImageBytes(t *testing.T) {
	temp := t.TempDir()
	relayRoot := filepath.Join(temp, "relay")
	relay := runtimeRelayCell(t, relayRoot, "1.0.0")
	client, serverTask := runtimeRelayServeClient(t, relay)
	lines := bufio.NewReader(client)
	image := []byte("\x89PNG\r\n\x1a\n")
	image = append(image, bytes.Repeat([]byte{29}, core.MaxImageBytes-len(image))...)
	reference, err := core.NewAttachmentRef(image, "image/png", 1024, 768)
	runtimeRelayMust(t, err)
	for _, protocolVersion := range []uint32{7, core.RelayProtocolVersion, core.RelayProtocolVersion} {
		response := runtimeRelayExchange(t, lines, client, core.RelayRequestEnvelope{
			RequestID:       "upload-photo",
			ProtocolVersion: protocolVersion,
			Request: core.RelayRequestInstallAttachment{
				Reference: reference,
				Data:      base64.StdEncoding.EncodeToString(image),
			},
		})
		if protocolVersion < 8 {
			if _, ok := response.Body.(core.RelayResponseBodyError); !ok {
				t.Fatalf("protocol %d installed an attachment: %#v", protocolVersion, response.Body)
			}
		} else if _, ok := runtimeRelayOk(t, response, "install the photo").(core.RelayResponsePayloadAttachmentInstalled); !ok {
			t.Fatalf("%#v", response.Body)
		}
	}
	response := runtimeRelayExchange(t, lines, client, core.RelayRequestEnvelope{
		RequestID:       "read-photo",
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         core.RelayRequestReadAttachment{Reference: reference},
	})
	data, ok := runtimeRelayOk(t, response, "image read failed").(core.RelayResponsePayloadAttachmentData)
	if !ok {
		t.Fatal("image read failed")
	}
	decoded, err := base64.StdEncoding.DecodeString(data.Data)
	runtimeRelayMust(t, err)
	if !bytes.Equal(decoded, image) {
		t.Fatal("the photo did not round-trip")
	}
	locked := relay.Lock()
	latest := locked.LatestOrdinal()
	locked.Unlock()
	if latest != 0 {
		t.Fatalf("image traffic was journaled: latest ordinal %d", latest)
	}
	info, err := os.Stat(filepath.Join(relayRoot, "attachments", reference.Sha256))
	runtimeRelayMust(t, err)
	if info.Size() != int64(core.MaxImageBytes) {
		t.Fatalf("stored %d bytes", info.Size())
	}
	client.Close()
	waitCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	served, joinErr := serverTask.Join(waitCtx)
	if joinErr != nil {
		t.Fatal(joinErr)
	}
	runtimeRelayMust(t, served)
}

// runtimeRelayRetryAssessmentIdentity is the cfg(test)
// DurableRelay::retry_assessment_identity: the current assessment's turn
// and completed ordinal.
func runtimeRelayRetryAssessmentIdentity(relay *DurableRelay) (string, uint64, bool) {
	assessment := relay.snapshot.Assessment
	if assessment == nil || !assessment.Current() {
		return "", 0, false
	}
	return assessment.TurnID, assessment.CompletedOrdinal, true
}

// runtimeRelayResolveRetryAssessment is the cfg(test)
// DurableRelay::resolve_retry_assessment: settle the named current
// assessment as a provider retry or as uncertain.
func runtimeRelayResolveRetryAssessment(relay *DurableRelay, id string, ordinal uint64, known bool, retryable bool) (bool, error) {
	if !known {
		return false, nil
	}
	current := relay.snapshot.Assessment
	if current == nil || current.TurnID != id || current.CompletedOrdinal != ordinal || !current.Current() {
		return false, nil
	}
	assessment := *current
	action := core.AssessmentActionUncertain
	assessment.Status = core.StatusAssessed
	if retryable {
		action = core.AssessmentActionRetryProvider
		assessment.Status = core.StatusScheduled
	}
	assessment.Action = &action
	if err := relay.storeAssessment(assessment); err != nil {
		return false, err
	}
	return retryable, nil
}

func TestCapacityRetryDispatchesWithoutAControllerAfterTheDeadline(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		temp := t.TempDir()
		state := runtimeRelayOpen(t, temp, "1.0.0")
		state.SetBackgroundWorkPolicy(BackgroundWorkPolicyAirTasks)
		setVerdictHarness(t, state, core.HarnessKindCodex)
		_, err := state.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
		runtimeRelayMust(t, err)
		runtimeRelayTestsSubmit(t, state, "capacity-original", runtimeRelayTestsPrompt("work"))
		claimed, err := state.ClaimPendingCommands(true)
		runtimeRelayMust(t, err)
		if len(claimed) == 0 || claimed[0].CommandID != "capacity-original" {
			t.Fatalf("claimed = %#v", claimed)
		}
		var update acpschema.SessionUpdate
		runtimeRelayMust(t, json.Unmarshal(runtimeRelayJSON(t, map[string]any{
			"sessionUpdate": "agent_message_chunk",
			"content":       map[string]any{"type": "text", "text": "Provider temporarily unavailable"},
		}), &update))
		_, err = state.RecordSessionUpdate(update)
		runtimeRelayMust(t, err)
		_, err = state.RecordCommandCompleted("capacity-original", core.RelayCommandOutcomePrompt{
			WorkFacts:  nil,
			Diagnostic: nil,
			StopReason: "error",
			Usage:      nil,
		})
		runtimeRelayMust(t, err)
		id, ordinal, known := runtimeRelayRetryAssessmentIdentity(state)
		resolved, err := runtimeRelayResolveRetryAssessment(state, id, ordinal, known, true)
		runtimeRelayMust(t, err)
		if !resolved {
			t.Fatal("the provider failure was not scheduled for a retry")
		}
		retry := state.OperationalState().CapacityRetry
		if retry == nil {
			t.Fatal("no capacity retry was armed")
		}
		facts := state.ActivityFacts()
		facts.CapacityRetryArmed = false
		if !core.IsQuiet(&facts) {
			t.Fatalf("%#v", facts)
		}
		configuredOrdinal := state.LatestOrdinal()
		relay := NewRelayCell(state)
		events, eventRx := runtimeEventChannel()
		wakes := newDispatchWakeChannel()
		commands := newCommandChannel(4)
		coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
		defer stop()
		runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
		for {
			locked := relay.Lock()
			latest := locked.LatestOrdinal()
			locked.Unlock()
			if latest > configuredOrdinal {
				break
			}
			runtime.Gosched()
		}
		// No controller wake or connected frontend is needed to drive the timer.
		time.Sleep(59 * time.Second)
		synctest.Wait()
		runtimeRelayNoCommand(t, commands)
		time.Sleep(2 * time.Second)
		assertPrompt(t, nextCommand(t, commands.receiver()), retry.CommandID, "Continue")
		runtimeRelayNoCommand(t, commands)
		runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
		runtimeRelayJoinOk(t, coordinator, "relay coordinator")
	})
}

// runtimeRelayRequestBody sends request on a daemon connection and returns
// its response body within five seconds.
func runtimeRelayRequestBody(t *testing.T, reader *bufio.Reader, writer io.Writer, request core.RelayRequest) core.RelayResponseBody {
	t.Helper()
	runtimeRelaySend(t, writer, core.RelayRequestEnvelope{
		RequestID:       "checkpoint-test",
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         request,
	})
	type read struct {
		line string
		err  error
	}
	done := make(chan read, 1)
	go func() {
		line, err := reader.ReadString('\n')
		done <- read{line: line, err: err}
	}()
	select {
	case result := <-done:
		if result.err != nil {
			t.Fatalf("read the daemon's response: %v", result.err)
		}
		return runtimeRelayDecode(t, result.line).Body
	case <-time.After(5 * time.Second):
		t.Fatal("the daemon did not answer")
		return nil
	}
}

func runtimeRelayStatusOf(t *testing.T, body core.RelayResponseBody) core.RelayOperationalState {
	t.Helper()
	ok, isOk := body.(core.RelayResponseBodyOk)
	if !isOk {
		t.Fatalf("%#v", body)
	}
	status, isStatus := ok.Payload.(core.RelayResponsePayloadStatus)
	if !isStatus {
		t.Fatalf("%#v", body)
	}
	return status.State
}

func TestCheckpointOnlyDaemonPreservesWorkAndSealsWithoutAHarness(t *testing.T) {
	temp := t.TempDir()
	root := filepath.Join(temp, "worker")
	durable := runtimeRelayOpen(t, root, "1.0.0")
	runtimeRelaySessionOpened(t, durable, "saved-native-session", true)
	runtimeRelayTestsSubmit(t, durable, "interrupted-turn", runtimeRelayTestsPrompt("working"))
	_, err := durable.ClaimPendingCommands(true)
	runtimeRelayMust(t, err)
	queuedText := strings.Repeat("preserved work ", 10_000)
	runtimeRelayTestsSubmit(t, durable, "queued-prompt", runtimeRelayTestsPrompt(queuedText))
	runtimeRelayTestsSubmit(t, durable, "queued-model", core.RelayCommandSetConfig{Key: "model", Value: "withdrawn-model"})
	shellMarker := filepath.Join(temp, "shell-ran")
	runtimeRelayTestsSubmit(t, durable, "queued-shell", core.RelayCommandRunUserShell{Command: "touch " + shellMarker})
	config := launchConfig("profile-that-must-not-be-used")
	config.RunMode = core.WorkerRunModeCheckpointOnly
	config.HarnessRuntime = core.HarnessRuntimePolicyManaged
	config.BridgeCommand = filepath.Join(temp, "nonexistent-harness")
	config.Cwd = temp
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	daemon := core.Spawn(func() error { return RunDaemon(ctx, root, config) })
	stream := runtimeRelayConnectWhileRunning(t, root, daemon, 10*time.Second, "checkpoint-only startup failed")
	defer stream.Close()
	reader := bufio.NewReader(stream)

	state := runtimeRelayStatusOf(t, runtimeRelayRequestBody(t, reader, stream, core.RelayRequestStatus{}))
	if !state.CheckpointOnly {
		t.Fatal("the daemon is not checkpoint-only")
	}
	if state.ACPReady == nil || *state.ACPReady {
		t.Fatalf("acp ready = %v", state.ACPReady)
	}
	if !runtimeRelayIs(state.NativeSessionID, "saved-native-session") {
		t.Fatalf("native session = %v", state.NativeSessionID)
	}
	queued := []string{}
	for _, prompt := range state.QueuedPrompts {
		queued = append(queued, prompt.CommandID)
	}
	// The interrupted turn is carried as a restart continuation ahead of
	// the work queued behind it.
	if want := []string{"auto-continue-restart-1-interrupted-turn", "queued-prompt", "queued-model"}; !slices.Equal(queued, want) {
		t.Fatalf("queued = %q, want %q", queued, want)
	}
	if _, ok := runtimeRelayRequestBody(t, reader, stream, core.RelayRequestSubmit{
		CommandID: "new-prompt",
		Command:   runtimeRelayTestsPrompt("must not run"),
	}).(core.RelayResponseBodyError); !ok {
		t.Fatal("a checkpoint-only worker accepted a new prompt")
	}
	if body := runtimeRelayRequestBody(t, reader, stream, core.RelayRequestSubmit{
		CommandID: "recovery-barrier",
		Command:   core.RelayCommandBeginCheckpoint{Reason: nil},
	}); !runtimeRelayIsOk(body) {
		t.Fatalf("%#v", body)
	}
	state = runtimeRelayStatusOf(t, runtimeRelayRequestBody(t, reader, stream, core.RelayRequestStatus{}))
	if state.CheckpointReady == nil {
		t.Fatal("the recovery barrier is not ready")
	}
	cursor := *state.CheckpointReady
	wrong := cursor
	wrong.Ordinal++
	if _, ok := runtimeRelayRequestBody(t, reader, stream, core.RelayRequestSubmit{
		CommandID: "bad-close",
		Command:   core.RelayCommandClose{BarrierCommandID: "recovery-barrier", Expected: wrong},
	}).(core.RelayResponseBodyError); !ok {
		t.Fatal("a close naming the wrong cut was accepted")
	}
	if body := runtimeRelayRequestBody(t, reader, stream, core.RelayRequestSubmit{
		CommandID: "recovery-close",
		Command:   core.RelayCommandClose{BarrierCommandID: "recovery-barrier", Expected: cursor},
	}); !runtimeRelayIsOk(body) {
		t.Fatalf("%#v", body)
	}
	completed := runtimeRelayRequestBody(t, reader, stream, core.RelayRequestSubmit{
		CommandID: "recovery-complete",
		Command:   core.RelayCommandCompleteCheckpoint{BarrierCommandID: "recovery-barrier"},
	})
	if !runtimeRelayIsOk(completed) {
		t.Fatalf("%#v", completed)
	}
	state = runtimeRelayStatusOf(t, runtimeRelayRequestBody(t, reader, stream, core.RelayRequestStatus{}))
	if state.Execution != core.RelayExecutionStateClosed {
		t.Fatalf("execution = %q", state.Execution)
	}
	if runtimeRelayExists(filepath.Join(root, "acp-supervisor.json")) {
		t.Fatal("a checkpoint-only worker started an ACP supervisor")
	}
	if runtimeRelayExists(shellMarker) {
		t.Fatal("a checkpoint-only worker ran a queued shell command")
	}
	runtimeRelayMust(t, stream.(*net.UnixConn).CloseWrite())
	select {
	case <-daemon.Done():
		t.Fatal("the daemon stopped before it was aborted")
	default:
	}
	runtimeRelayAbort(t, daemon, cancel)
	reopened, err := DurableRelayOpenForCheckpoint(root, runtimeRelayTestsSessionID, "1.0.0")
	runtimeRelayMust(t, err)
	if got := reopened.OperationalState().Execution; got != core.RelayExecutionStateClosed {
		t.Fatalf("reopened execution = %q", got)
	}
	if got := len(reopened.OperationalState().QueuedPrompts); got != 3 {
		t.Fatalf("reopened queue holds %d prompts", got)
	}
}

func runtimeRelayIsOk(body core.RelayResponseBody) bool {
	_, ok := body.(core.RelayResponseBodyOk)
	return ok
}

func runtimeRelayOpenForCheckpoint(t *testing.T, root string) *DurableRelay {
	t.Helper()
	relay, err := DurableRelayOpenForCheckpoint(root, runtimeRelayTestsSessionID, "1.0.0")
	runtimeRelayMust(t, err)
	return relay
}

func TestCheckpointOnlyRestartReleasesAnAbandonedBarrierAndPreservesAnAcceptedClose(t *testing.T) {
	temp := t.TempDir()
	runtimeRelayOpen(t, temp, "1.0.0")
	relay := runtimeRelayOpenForCheckpoint(t, temp)
	runtimeRelayTestsSubmit(t, relay, "abandoned-barrier", core.RelayCommandBeginCheckpoint{Reason: nil})
	runtimeRelayMust(t, relay.DispatchCheckpointOnly())
	if relay.OperationalState().CheckpointReady == nil {
		t.Fatal("the barrier is not ready")
	}
	relay = runtimeRelayOpenForCheckpoint(t, temp)
	if relay.OperationalState().CheckpointReady != nil {
		t.Fatal("a restart kept the abandoned barrier")
	}
	runtimeRelayTestsSubmit(t, relay, "verified-barrier", core.RelayCommandBeginCheckpoint{Reason: nil})
	runtimeRelayMust(t, relay.DispatchCheckpointOnly())
	cursor := relay.OperationalState().CheckpointReady
	if cursor == nil {
		t.Fatal("the verified barrier is not ready")
	}
	runtimeRelayTestsSubmit(t, relay, "accepted-close", core.RelayCommandClose{
		BarrierCommandID: "verified-barrier",
		Expected:         *cursor,
	})
	runtimeRelayMust(t, relay.DispatchCheckpointOnly())
	if got := relay.OperationalState().Execution; got != core.RelayExecutionStateClosing {
		t.Fatalf("execution = %q", got)
	}
	relay = runtimeRelayOpenForCheckpoint(t, temp)
	runtimeRelayMust(t, relay.DispatchCheckpointOnly())
	if got := relay.OperationalState().Execution; got != core.RelayExecutionStateClosed {
		t.Fatalf("execution = %q", got)
	}
	if ready := relay.OperationalState().ACPReady; ready == nil || *ready {
		t.Fatalf("acp ready = %v", ready)
	}
}

// runtimeRelayForkedPreExecChild is a child holding an inherited copy of a
// worker root's lock (Rust's root_owner::ForkedPreExecChild, which holds a
// forked child between fork and exec). Go cannot run code in that window, so
// the child inherits a duplicate of the lock descriptor outright and keeps it
// until it is released.
type runtimeRelayForkedPreExecChild struct {
	command *exec.Cmd
	control io.WriteCloser
}

func runtimeRelayForkedPreExecChildStart(t *testing.T, root string) *runtimeRelayForkedPreExecChild {
	t.Helper()
	inherited := runtimeRelayDescriptorsOf(t, filepath.Join(root, "worker.lock"))
	if len(inherited) == 0 {
		t.Fatal("the worker root owner holds no lock descriptor")
	}
	command := exec.Command("/bin/sh", "-c", "cat >/dev/null")
	command.ExtraFiles = inherited
	control, err := command.StdinPipe()
	runtimeRelayMust(t, err)
	runtimeRelayMust(t, command.Start())
	for _, file := range inherited {
		file.Close()
	}
	return &runtimeRelayForkedPreExecChild{command: command, control: control}
}

// releaseAfter lets the child finish after delay; the returned task reports
// the child's exit.
func (c *runtimeRelayForkedPreExecChild) releaseAfter(delay time.Duration) *core.JoinHandle[error] {
	return core.Spawn(func() error {
		time.Sleep(delay)
		c.control.Close()
		return c.command.Wait()
	})
}

// Hard-won: #1257: a test-spawned child can retain the released worker flock until exec.
func TestCheckpointOnlyStartRefusesMissingOrCorruptStateWithoutStartingAHarness(t *testing.T) {
	temp := t.TempDir()
	root := filepath.Join(temp, "worker")
	config := launchConfig("unused-profile")
	config.RunMode = core.WorkerRunModeCheckpointOnly
	config.Cwd = temp
	err := RunDaemon(context.Background(), root, runtimeRelayCloneLaunch(config))
	if err == nil || !strings.Contains(err.Error(), "existing relay state") {
		t.Fatalf("%v", err)
	}
	state := filepath.Join(root, core.RelayStateFile)
	runtimeRelayWriteFile(t, state, []byte("corrupt state"))
	owner, err := WorkerRootOwnerAcquire(root)
	runtimeRelayMust(t, err)
	child := runtimeRelayForkedPreExecChildStart(t, root)
	owner.Release()
	release := child.releaseAfter(30 * time.Millisecond)
	owner, err = WorkerRootOwnerAcquire(root)
	runtimeRelayMust(t, err)
	defer owner.Release()
	err = RunDaemonOwned(context.Background(), owner, config)
	if exit := runtimeRelayJoin(t, release, "release the forked child"); exit != nil {
		t.Fatalf("the forked child failed: %v", exit)
	}
	if err == nil || !strings.Contains(err.Error(), "parse") {
		t.Fatalf("%v", err)
	}
	if got := runtimeRelayReadFile(t, state); string(got) != "corrupt state" {
		t.Fatalf("state = %q", got)
	}
	if runtimeRelayExists(filepath.Join(root, "control.sock")) {
		t.Fatal("a refused start bound its control socket")
	}
	if runtimeRelayExists(filepath.Join(root, "acp-supervisor.json")) {
		t.Fatal("a refused start wrote an ACP supervisor spec")
	}
}

// runtimeRelayClosedWriter is a duplex end whose reader is gone (Rust:
// tokio::io::duplex with the reading end dropped): every write fails.
func runtimeRelayClosedWriter() io.Writer {
	reader, writer := io.Pipe()
	reader.Close()
	return writer
}

// Hard-won: 630dee318: File polling imposed a five-second startup budget on a correct process tree.
func TestAcpSupervisorTerminatesDescendantsWhenParentInputEnds(t *testing.T) {
	for _, brokenOutput := range []bool{false, true} {
		func() {
			root := t.TempDir()
			marker := filepath.Join(root, "descendant-writes")
			readyPath := filepath.Join(root, "ready.sock")
			ready, err := net.ListenUnix("unix", &net.UnixAddr{Name: readyPath, Net: "unix"})
			runtimeRelayMust(t, err)
			defer ready.Close()
			defer func() {
				if text, err := os.ReadFile(filepath.Join(root, "descendant-writes.pid")); err == nil {
					var pid int
					if _, err := fmt.Sscanf(string(text), "%d", &pid); err == nil {
						core.TerminateProcessGroup(pid, syscall.SIGKILL)
					}
				}
			}()
			spec := AcpSupervisorSpec{
				Command: "python3",
				Args: []string{
					"-c",
					`
import os, signal, socket, sys, time
with open(sys.argv[1] + '.pid', 'w') as f: f.write(str(os.getpid()))
read_ready, write_ready = os.pipe()
if os.fork() == 0:
    os.close(read_ready)
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    with open(sys.argv[1], 'a') as f: f.write('x')
    with socket.socket(socket.AF_UNIX) as ready:
        ready.connect(sys.argv[2])
        ready.sendall(b'x')
    os.write(write_ready, b'x')
    os.close(write_ready)
    while True:
        with open(sys.argv[1], 'a') as f: f.write('x')
        time.sleep(.01)
# Do not trigger broken-output cleanup before the child has signalled readiness.
os.close(write_ready)
assert os.read(read_ready, 1) == b'x'
os.close(read_ready)
print('x' * 100000, flush=True)
while True: time.sleep(1)
`,
					marker,
					readyPath,
				},
				Environment:         map[string]string{},
				ExcludedEnvironment: []string{},
				Cwd:                 root,
				HarnessLease:        nil,
				NodeLease:           nil,
			}
			input, held := io.Pipe()
			defer held.Close()
			var output io.Writer = io.Discard
			if brokenOutput {
				output = runtimeRelayClosedWriter()
			}
			task := core.Spawn(func() error {
				return runACPSupervisorWithStreams(context.Background(), spec, input, output)
			})
			readiness := make(chan error, 1)
			go func() {
				child, err := ready.Accept()
				if err != nil {
					readiness <- err
					return
				}
				defer child.Close()
				signal := make([]byte, 1)
				if _, err := io.ReadFull(child, signal); err != nil || signal[0] != 'x' {
					readiness <- fmt.Errorf("readiness signal %q: %v", signal, err)
					return
				}
				readiness <- nil
			}()
			select {
			case err := <-readiness:
				runtimeRelayMust(t, err)
			case <-task.Done():
				t.Fatalf("the supervisor stopped before its descendant was ready: %s", runtimeRelayTaskEnding(task))
			case <-time.After(ioTimeout):
				t.Fatal("timed out waiting for descendant readiness")
			}
			if !brokenOutput {
				held.Close()
			}
			waitCtx, cancel := context.WithTimeout(context.Background(), ioTimeout)
			defer cancel()
			result, joinErr := task.Join(waitCtx)
			if joinErr != nil {
				t.Fatalf("broken output %v: %v", brokenOutput, joinErr)
			}
			if brokenOutput {
				if result == nil || !strings.Contains(result.Error(), "forward ACP supervisor output") {
					t.Fatalf("%v", result)
				}
			} else if result != nil {
				t.Fatal(result)
			}
			time.Sleep(50 * time.Millisecond)
			info, err := os.Stat(marker)
			runtimeRelayMust(t, err)
			size := info.Size()
			time.Sleep(100 * time.Millisecond)
			info, err = os.Stat(marker)
			runtimeRelayMust(t, err)
			if info.Size() != size {
				t.Fatal("descendant must stop before working files are removed")
			}
		}()
	}
}

// macOS caps a Unix socket address at 104 bytes, and worker roots under
// `<data_dir>/workers/<session id>/` routinely pass that. The reviewer and
// sub-agent sockets only bind under their own configuration, so proving that
// `control.sock` binds and serves is enough to show the daemon no longer
// trips over `sun_path`.
//
// The daemon runs checkpoint-only so that the test owns its lifetime. A
// daemon that starts a harness returns as soon as the bridge fails to
// launch, and under load that return could drop the listener before the
// status request was served, resetting the connection.
// Hard-won: d1744be0: normal worker roots exceeded sockaddr_un.sun_path and could not bind a socket.
func TestAWorkerBindsItsSocketsUnderARootLongerThanSunPath(t *testing.T) {
	temp := t.TempDir()
	root := temp
	for len(filepath.Join(root, "control.sock")) <= 120 {
		root = filepath.Join(root, "nested-worker-root-component")
	}
	runtimeRelayMkdirAll(t, root)
	if len(filepath.Join(root, "control.sock")) <= core.UnixSocketPathLimit() {
		t.Fatal("the test root must be long enough to need the relative-name bind")
	}

	// Checkpoint-only startup refuses a root with no durable relay state, so
	// record one observation to write it.
	durable := runtimeRelayOpen(t, root, "1.0.0")
	runtimeRelaySessionOpened(t, durable, "saved-native-session", true)

	config := launchConfig("profile-home-that-must-not-be-used")
	config.RunMode = core.WorkerRunModeCheckpointOnly
	config.Cwd = temp
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	daemon := core.Spawn(func() error { return RunDaemon(ctx, root, config) })

	socket := filepath.Join(root, "control.sock")
	var stream *net.UnixConn
	deadline := time.Now().Add(10 * time.Second)
	for {
		connected, err := core.ConnectUnixStream(socket)
		if err == nil {
			stream = connected
			break
		}
		select {
		case <-daemon.Done():
			t.Fatal("the daemon stopped before binding a long control socket path")
		default:
		}
		if !time.Now().Before(deadline) {
			t.Fatalf("no control socket within 10s: %v", err)
		}
		time.Sleep(10 * time.Millisecond)
	}
	defer stream.Close()

	response := runtimeRelayExchange(t, bufio.NewReader(stream), stream, core.RelayRequestEnvelope{
		RequestID:       "status-over-a-long-socket-path",
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         core.RelayRequestStatus{},
	})
	if _, ok := runtimeRelayOk(t, response, "a worker on a long root did not serve status").(core.RelayResponsePayloadStatus); !ok {
		t.Fatalf("a worker on a long root did not serve status: %#v", response.Body)
	}

	runtimeRelayMust(t, stream.CloseWrite())
	select {
	case <-daemon.Done():
		t.Fatal("the daemon must still have been serving when the test aborted it")
	default:
	}
	runtimeRelayAbort(t, daemon, cancel)
}

// repositoryWhoseStagingBlocks builds a repository (in a directory of
// t.TempDir) whose staging blocks for seconds, by giving every path a clean
// filter that sleeps. This is how a working tree with hundreds of thousands
// of untracked files behaves for a review capture, at a size a test can
// hold.
//
// The tracked file is left modified on purpose. A capture only runs the
// clean filter over the dirty tracked paths it stages, so a fixture whose
// tracked files are all clean blocks only when Git happens to re-hash a
// racily-clean entry, and the wait it is supposed to prove disappears on a
// fast machine.
func repositoryWhoseStagingBlocks(t *testing.T, seconds uint32) string {
	t.Helper()
	repository := t.TempDir()
	testSupportGit(repository, "init", "-q", ".")
	testSupportGit(repository, "config", "user.email", "test@example.invalid")
	testSupportGit(repository, "config", "user.name", "Test")
	runtimeRelayWriteFile(t, filepath.Join(repository, "tracked.txt"), []byte("tracked\n"))
	testSupportGit(repository, "add", "tracked.txt")
	testSupportGit(repository, "commit", "-qm", "seed")
	testSupportGit(repository, "config", "filter.slow.clean", fmt.Sprintf("sleep %d && cat", seconds))
	runtimeRelayWriteFile(t, filepath.Join(repository, ".gitattributes"), []byte("* filter=slow\n"))
	runtimeRelayWriteFile(t, filepath.Join(repository, "untracked.txt"), []byte("untracked\n"))
	runtimeRelayWriteFile(t, filepath.Join(repository, "tracked.txt"), []byte("tracked, and edited since\n"))
	return repository
}

// workerStatus asks the worker serving root's control socket for its
// operational state.
func workerStatus(ctx context.Context, root string) (core.RelayOperationalState, error) {
	var dialer net.Dialer
	stream, err := dialer.DialContext(ctx, "unix", filepath.Join(root, "control.sock"))
	if err != nil {
		return core.RelayOperationalState{}, fmt.Errorf("connect: %v", err)
	}
	defer stream.Close()
	encoded, err := json.Marshal(core.RelayRequestEnvelope{
		RequestID:       "startup-status",
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         core.RelayRequestStatus{},
	})
	if err != nil {
		return core.RelayOperationalState{}, fmt.Errorf("encode: %v", err)
	}
	if _, err := stream.Write(append(encoded, '\n')); err != nil {
		return core.RelayOperationalState{}, fmt.Errorf("write: %v", err)
	}
	if err := stream.SetReadDeadline(time.Now().Add(250 * time.Millisecond)); err != nil {
		return core.RelayOperationalState{}, fmt.Errorf("read: %v", err)
	}
	line, err := bufio.NewReader(stream).ReadString('\n')
	if err != nil {
		if errors.Is(err, os.ErrDeadlineExceeded) {
			return core.RelayOperationalState{}, errors.New("timed out waiting for Status")
		}
		if !errors.Is(err, io.EOF) {
			return core.RelayOperationalState{}, fmt.Errorf("read: %v", err)
		}
	}
	var response core.RelayResponseEnvelope
	if err := json.Unmarshal([]byte(line), &response); err != nil {
		return core.RelayOperationalState{}, fmt.Errorf("decode: %v; line=%q", err, line)
	}
	if body, ok := response.Body.(core.RelayResponseBodyOk); ok {
		if status, ok := body.Payload.(core.RelayResponsePayloadStatus); ok {
			return status.State, nil
		}
	}
	return core.RelayOperationalState{}, fmt.Errorf("Status returned %#v", response.Body)
}

// startupStepRecorded is whether the worker's startup record already names
// step.
//
// The record outlives the worker, which makes it the reliable witness for a
// step whose visible effect can vanish first: a worker whose harness exits
// at once removes `control.sock` again during teardown.
func startupStepRecorded(root, step string) bool {
	return slices.Contains(runtimeRelayStartupSteps(root), step)
}

// runtimeRelayStartupSteps is every step the worker's startup record names.
func runtimeRelayStartupSteps(root string) []string {
	bytes, err := os.ReadFile(filepath.Join(root, core.WorkerStartupFile))
	if err != nil {
		return nil
	}
	var record any
	if json.Unmarshal(bytes, &record) != nil {
		return nil
	}
	object, _ := record.(map[string]any)
	recorded, _ := object["steps"].([]any)
	steps := []string{}
	for _, entry := range recorded {
		fields, _ := entry.(map[string]any)
		if name, ok := fields["step"].(string); ok {
			steps = append(steps, name)
		}
	}
	return steps
}

// A session that no review can run for must do no working-tree capture at
// startup. This is the #1065 failure: the capture is proportional to the
// working tree, so a session in a large tree never reached its control
// socket, and a sub-agent child paid that cost for a review it is never given.
// Hard-won: 105769b9: a child in a 524GB workspace never bound its socket because startup always captured Git state.
func TestASessionWithoutReviewCaptureBindsItsSocketInATreeThatCannotBeStaged(t *testing.T) {
	// A capture would take a minute, so the startup record can prove the
	// worker published its relay before doing any working-tree capture.
	workspace := repositoryWhoseStagingBlocks(t, 60)
	temp := t.TempDir()
	root := filepath.Join(temp, "worker")
	runtimeRelayMkdirAll(t, root)

	config := launchConfig("profile-home")
	config.Cwd = workspace
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	daemon := core.Spawn(func() error { return RunDaemon(ctx, root, config) })

	appeared := false
	deadline := time.Now().Add(30 * time.Second)
	for time.Now().Before(deadline) {
		// `control.sock` alone is a momentary witness: the fake harness
		// exits at once, so the worker can bind, serve, and remove the
		// socket between two polls. `serving` is recorded only after the
		// bind succeeded, and that record survives the worker.
		if runtimeRelayExists(filepath.Join(root, "control.sock")) || startupStepRecorded(root, "serving") {
			appeared = true
			break
		}
		select {
		case <-daemon.Done():
			deadline = time.Time{}
		default:
			time.Sleep(20 * time.Millisecond)
		}
	}
	// The startup record names the step a slow start stalled in.
	steps, _ := os.ReadFile(filepath.Join(root, core.WorkerStartupFile))
	if !appeared {
		ending := "still running"
		select {
		case <-daemon.Done():
			ending = runtimeRelayTaskEnding(daemon)
		default:
		}
		t.Fatalf("an unreviewed session must not wait for a working-tree capture; startup steps: %s; worker: %s", steps, ending)
	}
	runtimeRelayAbort(t, daemon, cancel)
}

// #1192: the worker answers the daemon during harness preparation, and the
// status response names the step currently owned by the preparation task.
func TestTheControlSocketAnswersWhileTheHarnessIsPrepared(t *testing.T) {
	temp := t.TempDir()
	root := filepath.Join(temp, "worker")
	runtimeRelayMkdirAll(t, root)
	// Selecting a built-in Codex bridge probes its version before ACP starts.
	slowHarness := filepath.Join(temp, "codex-acp")
	runtimeRelayWriteFile(t, slowHarness, []byte(fmt.Sprintf(
		"#!/bin/sh\nif [ \"$1\" = --version ]; then /bin/sleep 3; echo '@brokkai/codex-acp %s'; exit 0; fi\nexit 1\n",
		core.CodexACPVersion,
	)))
	runtimeRelayMust(t, os.Chmod(slowHarness, 0o755))

	config := launchConfig("profile-home")
	config.BridgeCommand = "codex-acp"
	config.Environment["PATH"] = temp + ":/usr/bin:/bin"
	config.Cwd = temp
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	daemon := core.Spawn(func() error { return RunDaemon(ctx, root, config) })

	var recorded []string
	var status core.RelayOperationalState
	served := false
	deadline := time.Now().Add(30 * time.Second)
	for !served {
		if !time.Now().Before(deadline) {
			t.Fatal("the worker neither served nor stopped")
		}
		recorded = runtimeRelayStartupSteps(root)
		preparing := slices.Contains(recorded, "harness-resolve") && !slices.Contains(recorded, "bridge-start")
		if preparing {
			if state, err := workerStatus(ctx, root); err == nil {
				status = state
				served = true
				break
			}
		}
		select {
		case <-daemon.Done():
			t.Fatalf("worker stopped before serving during harness preparation: %s", runtimeRelayTaskEnding(daemon))
		default:
		}
		time.Sleep(10 * time.Millisecond)
	}
	preparation, ok := status.HarnessPreparation.(core.HarnessPreparationPreparing)
	if !ok {
		if pointer, isPointer := status.HarnessPreparation.(*core.HarnessPreparationPreparing); isPointer && pointer != nil {
			preparation, ok = *pointer, true
		}
	}
	if !ok || preparation.Step != "harness-resolve" {
		t.Fatalf("harness preparation = %#v", status.HarnessPreparation)
	}
	serving := slices.Index(recorded, "serving")
	resolving := slices.Index(recorded, "harness-resolve")
	if serving < 0 || resolving < 0 || serving >= resolving {
		t.Fatalf("worker did not publish before harness preparation: %q", recorded)
	}
	select {
	case <-daemon.Done():
		t.Fatal("the daemon stopped before it was aborted")
	default:
	}
	runtimeRelayAbort(t, daemon, cancel)
}

// #1617: a harness that cannot be prepared ends the worker with the typed
// startup failure its exit record carries, so the daemon stops restarting it
// and shows why. It no longer leaves a relay that answers with no harness.
// Hard-won: #1617: a failed preparation left a live worker that could never run a turn.
func TestAHarnessThatCannotBePreparedEndsTheWorkerWithATypedFailure(t *testing.T) {
	temp := t.TempDir()
	root := filepath.Join(temp, "worker")
	runtimeRelayMkdirAll(t, root)
	config := launchConfig("profile-home")
	// Resolving a bridge that does not exist fails harness preparation.
	config.BridgeCommand = filepath.Join(temp, "missing-bridge")
	config.Cwd = temp
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	daemon := core.Spawn(func() error { return RunDaemon(ctx, root, config) })
	waitCtx, stopWaiting := context.WithTimeout(context.Background(), 60*time.Second)
	defer stopWaiting()
	err, joinErr := daemon.Join(waitCtx)
	if joinErr != nil {
		t.Fatalf("a failed preparation must end the worker: %v", joinErr)
	}
	if err == nil {
		t.Fatal("a failed preparation must end the worker with an error")
	}
	var failure core.WorkerHarnessStartupFailure
	if !errors.As(err, &failure) {
		var pointer *core.WorkerHarnessStartupFailure
		if !errors.As(err, &pointer) || pointer == nil {
			t.Fatalf("untyped preparation failure: %v", err)
		}
		failure = *pointer
	}
	if !strings.HasPrefix(failure.Message, "harness preparation failed at harness-resolve: ") {
		t.Fatalf("%#v", failure)
	}
	if runtimeRelayExists(filepath.Join(root, "control.sock")) {
		t.Fatal("the failed worker left its control socket behind")
	}
}

// coordinatorBehindAReadyBarrier starts a coordinator whose session has a
// ready checkpoint barrier, fed by a runtime event channel of capacity.
// Returns the cut the barrier holds.
func coordinatorBehindAReadyBarrier(t *testing.T, temp string, capacity int) (*RelayCell, chan<- core.RuntimeEvent, *dispatchWakeChannel, *core.JoinHandle[error], core.RelayCursor) {
	t.Helper()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "checkpoint-barrier", core.RelayCommandBeginCheckpoint{Reason: nil})
	relay := NewRelayCell(durable)
	events := make(chan core.RuntimeEvent, capacity)
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, events, wakes, commands)
	t.Cleanup(stop)
	events <- core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}}
	waitUntil(t, func() bool { return runtimeRelayOperationalState(relay).CheckpointReady != nil },
		"the checkpoint barrier never became ready")
	cut := *runtimeRelayOperationalState(relay).CheckpointReady
	return relay, events, wakes, coordinator, cut
}

func journaledNotices(relay *RelayCell) []string {
	locked := relay.Lock()
	defer locked.Unlock()
	events, err := locked.EventsAfter(0, core.RelayEventGenesisDigest)
	if err != nil {
		panic(err)
	}
	notices := []string{}
	for _, event := range events {
		if notice, ok := event.Observation.(core.RelayObservationNotice); ok {
			notices = append(notices, notice.Message)
		}
	}
	return notices
}

// Holding the worker's writes behind a ready barrier must not stop the
// coordinator reading runtime events: a full bounded channel parks the ACP
// runtime, and a parked runtime cannot act on anything, including the reply
// to a question sent through the command channel. The coordinator keeps
// draining; the relay keeps what can wait in memory and journals it, in
// order, when the barrier ends.
// Hard-won: 634af0a3: a ready checkpoint barrier could stall ACP event draining on its bounded channel.
func TestAReadyBarrierKeepsDrainingRuntimeEventsPastTheChannelCapacity(t *testing.T) {
	temp := t.TempDir()
	relay, events, _, coordinator, cut := coordinatorBehindAReadyBarrier(t, temp, 4)

	sent := []string{}
	for index := range 64 {
		sent = append(sent, fmt.Sprintf("held notice %d", index))
	}
	for _, message := range sent {
		select {
		case events <- core.RuntimeEventNotice{Message: message}:
		case <-time.After(5 * time.Second):
			t.Fatal("the coordinator stopped draining runtime events behind the barrier")
		}
	}
	// Rust waits for the channel's capacity to return. Its single-threaded
	// test runtime finishes handling a received event before yielding, so a
	// drained channel there means every notice is held. A goroutine receives
	// first and takes the relay after, so here wait for the relay to hold
	// all of them too.
	waitUntil(t, func() bool {
		if len(events) != 0 {
			return false
		}
		observed := relay.Observe()
		defer observed.Unlock()
		return len(observed.heldWrites) == len(sent)
	}, "the coordinator never drained the channel")
	if got := runtimeRelayOperationalState(relay).LatestOrdinal; got != cut.Ordinal {
		t.Fatalf("latest ordinal = %d, cut at %d", got, cut.Ordinal)
	}
	if notices := journaledNotices(relay); len(notices) != 0 {
		t.Fatalf("held notices were journaled: %q", notices)
	}

	runtimeRelaySubmitLocked(t, relay, "checkpoint-release", core.RelayCommandReleaseCheckpoint{BarrierCommandID: "checkpoint-barrier"})
	if notices := journaledNotices(relay); !slices.Equal(notices, sent) {
		t.Fatalf("notices = %q, want %q", notices, sent)
	}

	close(events)
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

// A question the harness asks while a barrier holds its cut is journaled at
// once, so a person can see and answer it; the cut is spoiled, as any sign of
// the agent at work spoils it. The answer travels to the runtime through the
// command channel, which the hold never touches.
func TestAQuestionDuringAReadyBarrierIsJournaledAtOnce(t *testing.T) {
	temp := t.TempDir()
	relay, events, _, coordinator, cut := coordinatorBehindAReadyBarrier(t, temp, 16)
	events <- core.RuntimeEventNotice{Message: "before the question"}
	events <- core.RuntimeEventElicitationRequested{Request: core.ElicitationRequest{
		ID:             "question-1",
		Message:        "May I continue?",
		Title:          nil,
		Description:    nil,
		Fields:         []core.ElicitationField{},
		ToolPermission: nil,
	}}
	asked := func() (uint64, bool) {
		for _, event := range runtimeRelayLockedEvents(t, relay) {
			if requested, ok := event.Observation.(core.RelayObservationElicitationRequested); ok && requested.Request.ID == "question-1" {
				return event.Ordinal, true
			}
		}
		return 0, false
	}
	waitUntil(t, func() bool {
		_, ok := asked()
		return ok
	}, "the question never reached the journal")
	if ordinal, _ := asked(); ordinal <= cut.Ordinal {
		t.Fatalf("question at %d, cut at %d", ordinal, cut.Ordinal)
	}
	if notices := journaledNotices(relay); !slices.Equal(notices, []string{"before the question"}) {
		t.Fatalf("notices = %q", notices)
	}
	locked := relay.Lock()
	held := locked.WorkerWritesHeld()
	locked.Unlock()
	if held {
		t.Fatal("the question left the worker's writes held")
	}

	close(events)
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

func TestExpiredMailboxHookLeaseStartsIdleWakeWithoutAnotherRequest(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	waitUntil(t, func() bool {
		return runtimeRelayHasObservation[core.RelayObservationSessionConfigured](runtimeRelayLockedEvents(t, relay))
	}, "the relay coordinator did not configure its session")

	runtimeRelaySubmitLocked(t, relay, "lease-timer-active-prompt", runtimeRelayTestsPrompt("active"))
	runtimeRelayMust(t, wakeDispatch(relay, wakes))
	assertPrompt(t, nextCommand(t, commands.receiver()), "lease-timer-active-prompt", "active")

	event := core.MailboxEvent{
		Key:         "parent:message:lease-timer",
		Source:      "parent",
		Wake:        true,
		CreatedAtMs: uint64(max(core.EpochMillis(), 0)),
		Body:        core.MailboxEventBodyParentMessage{Text: "Wake after the hook lease expires."},
	}
	{
		locked := relay.Lock()
		runtimeRelayTestsSubmit(t, locked.DurableRelay, "mailbox-lease-timer-event", core.RelayCommandDeliverMailboxEvent{Event: event})
		_, err := locked.RecordObservation(core.RelayObservationMailboxHookLeaseCreated{Lease: core.MailboxHookLease{
			LeaseID:     "mailbox-hook-timer-test",
			Events:      []core.MailboxEvent{event},
			HookEvent:   "PostToolUse",
			ExpiresAtMs: core.EpochMillis() + 2_000,
			Steering:    nil,
		}})
		locked.Unlock()
		runtimeRelayMust(t, err)
	}
	runtimeRelayMust(t, wakeDispatch(relay, wakes))
	runtimeRelayEmit(t, events, core.RuntimeEventPromptFinished{
		Diagnostic: nil,
		RequestID:  "lease-timer-active-prompt",
		StopReason: "end_turn",
		Usage:      nil,
	})
	waitUntil(t, func() bool {
		return slices.ContainsFunc(runtimeRelayLockedEvents(t, relay), func(record core.RelayEvent) bool {
			completed, ok := record.Observation.(core.RelayObservationCommandCompleted)
			return ok && completed.CommandID == "lease-timer-active-prompt"
		})
	}, "the active turn did not finish before the mailbox lease expired")
	{
		locked := relay.Lock()
		deadline, ok := locked.MailboxHookLeaseDeadline()
		locked.Unlock()
		if !ok || deadline <= core.EpochMillis() {
			t.Fatal("the turn must finish while the hook lease is still live")
		}
	}

	wake := runtimeRelayCommandWithin(t, commands, 5*time.Second)
	prompt, ok := wake.(CommandRequestPrompt)
	if !ok {
		t.Fatalf("expected the mailbox wake to dispatch as a prompt, got %#v", wake)
	}
	if !strings.HasPrefix(prompt.RequestID, "mailbox-wake-") {
		t.Fatalf("wake = %q", prompt.RequestID)
	}

	events.close()
	runtimeRelayJoinOk(t, coordinator, "relay coordinator")
}

func TestCpuUsageAnswersFromTheSamplerWithoutJournaling(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "1.0.0")
	wake := newDispatchWakeChannel()
	fatal := make(chan error, 1)
	cpu := core.NewWatch(cpuRead{})
	server, client := runtimeRelayPair(t)
	connection := defaultConnectionRuntime()
	connection.cpu = cpu
	task := core.Spawn(func() error {
		return serveClientWithMemory(context.Background(), server, relay, wake,
			credentialEndpointResult{err: errors.New("no credentials")}, connection, fatal)
	})
	lines := bufio.NewReader(client)
	usage := core.SessionCpuUsage{
		Scope:             core.CpuSampleScopeProcessTree,
		RecentPermille:    230,
		HourlyPermille:    100,
		HourlyCoveredSecs: 20,
		OnlineCpus:        4,
	}
	for _, value := range []cpuRead{
		{Usage: nil},
		{Usage: &usage},
		{Err: errors.New("measurement denied")},
	} {
		cpu.SendReplace(value)
		runtimeRelaySend(t, client, core.RelayRequestEnvelope{
			RequestID:       "cpu",
			ProtocolVersion: core.RelayProtocolVersion,
			Request:         core.RelayRequestCpuUsage{},
		})
		read := make(chan string, 1)
		go func() {
			line, _ := lines.ReadString('\n')
			read <- line
		}()
		var line string
		select {
		case line = <-read:
		case <-time.After(2 * time.Second):
			t.Fatal("no CPU usage answer")
		}
		response := runtimeRelayDecode(t, line)
		switch body := response.Body.(type) {
		case core.RelayResponseBodyOk:
			answer, ok := body.Payload.(core.RelayResponsePayloadCpuUsage)
			if value.Err != nil || !ok || !reflect.DeepEqual(answer.Usage, value.Usage) {
				t.Fatalf("value %#v answered %#v", value, response.Body)
			}
		case core.RelayResponseBodyError:
			if value.Err == nil || body.Error.Message != value.Err.Error() || body.Error.Retryable {
				t.Fatalf("value %#v answered %#v", value, response.Body)
			}
		default:
			t.Fatalf("%#v", response.Body)
		}
		locked := relay.Lock()
		latest := locked.LatestOrdinal()
		locked.Unlock()
		if latest != 0 {
			t.Fatalf("CPU usage was journaled: latest ordinal %d", latest)
		}
	}
	client.Close()
	runtimeRelayJoinOk(t, task, "serve the CPU usage connection")
}

// runCapturingCoordinator is a sub-agent's coordinator: it observes its
// agent's work when a prompt ends.
func runCapturingCoordinator(ctx context.Context, relay *RelayCell, events <-chan core.RuntimeEvent, wakes *dispatchWakeChannel, commands *commandChannel, observer WorkObserver) error {
	cwd, err := os.Getwd()
	if err != nil {
		return err
	}
	shellEvents := make(chan core.RuntimeEvent, 1)
	shells := NewUserShellRegistry(cwd, map[string]string{}, shellEvents)
	return runRelayCoordinatorWithVerdict(ctx, relay, events, wakes, commands, shells, nil, nil, &observer)
}

// runtimeRelaySpawnCapturingCoordinator spawns a capturing coordinator;
// cancel stops it (Rust: abort).
func runtimeRelaySpawnCapturingCoordinator(t *testing.T, relay *RelayCell, events <-chan core.RuntimeEvent, wakes *dispatchWakeChannel, commands *commandChannel, observer WorkObserver) *core.JoinHandle[error] {
	t.Helper()
	ctx, cancel := context.WithCancel(context.Background())
	t.Cleanup(cancel)
	return core.Spawn(func() error {
		return runCapturingCoordinator(ctx, relay, events, wakes, commands, observer)
	})
}

// promptOutcomeFacts is the facts a prompt's published outcome carries, once
// it is published: published false is Rust's None, and a nil facts with
// published true is an outcome without any (Some(None)).
func promptOutcomeFacts(relay *RelayCell) (facts *core.WorkFacts, published bool) {
	locked := relay.Lock()
	defer locked.Unlock()
	events, err := locked.EventsAfter(0, core.RelayEventGenesisDigest)
	if err != nil {
		panic(err)
	}
	for _, event := range events {
		if outcome, ok := runtimeRelayPromptOutcome(event); ok {
			return outcome.WorkFacts, true
		}
	}
	return nil, false
}

// gatedGit is a fake `git` whose `status` announces itself and then waits
// for a release file (or forever when release is false), so a test can hold
// the capture open.
func gatedGit(t *testing.T, directory string, release bool) string {
	t.Helper()
	git := filepath.Join(directory, "gated-git")
	wait := `while true; do sleep 0.05; done`
	if release {
		wait = `while [ ! -f "$dir/release" ]; do sleep 0.01; done`
	}
	script := fmt.Sprintf(`#!/bin/sh
dir="$2"; command="$3"
sha=1111111111111111111111111111111111111111
case "$command" in
  status) touch "$dir/status.started"; %s
          printf '# branch.oid %%s\000# branch.head main\000? notes.txt\000' "$sha" ;;
  log) printf '%%s\000The subject\n' "$sha" ;;
  *) exit 2 ;;
esac
`, wait)
	runtimeRelayWriteFile(t, git, []byte(script))
	runtimeRelayMust(t, os.Chmod(git, 0o755))
	return git
}

func captureObserver(relay *RelayCell, directory string, git string) WorkObserver {
	locked := relay.Lock()
	defer locked.Unlock()
	return WorkObserver{
		SessionID:      runtimeRelayTestsSessionID,
		WorkerPID:      uint32(os.Getpid()),
		Directory:      directory,
		Git:            git,
		HarnessReadyMs: locked.HarnessReadyClock(),
		Children:       &ObserverChildren{},
	}
}

// runtimeRelayWaitForCaptureStart waits for the gated Git read to begin.
func runtimeRelayWaitForCaptureStart(work string) {
	testSupportWaitFor("the capture to start", func() struct{} {
		for !runtimeRelayExists(filepath.Join(work, "status.started")) {
			runtime.Gosched()
		}
		return struct{}{}
	})
}

// runtimeRelayWaitForOutcome waits until the prompt's outcome is published.
func runtimeRelayWaitForOutcome(relay *RelayCell) {
	testSupportWaitFor("the outcome to be published", func() struct{} {
		for {
			if _, published := promptOutcomeFacts(relay); published {
				return struct{}{}
			}
			runtime.Gosched()
		}
	})
}

// A sub-agent's prompt outcome is published with the facts its worker
// observed, and the next prompt, which this task dispatches only after the
// outcome is recorded, waits for the observation.
func TestASubAgentsPromptOutcomeCarriesItsFactsAndTheNextPromptWaitsForThem(t *testing.T) {
	temp := t.TempDir()
	work := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "prompt-first", runtimeRelayTestsPrompt("first"))
	runtimeRelayTestsSubmit(t, durable, "prompt-second", runtimeRelayTestsPrompt("second"))
	relay := NewRelayCell(durable)
	git := gatedGit(t, work, true)
	observer := captureObserver(relay, work, git)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator := runtimeRelaySpawnCapturingCoordinator(t, relay, eventRx, wakes, commands, observer)
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-first", "first")
	runtimeRelayEmit(t, events, core.RuntimeEventPromptFinished{
		Diagnostic: nil,
		RequestID:  "prompt-first",
		StopReason: "end_turn",
		Usage:      nil,
	})
	// The capture is running (its Git read has started and is held): nothing is
	// published and the second prompt has not been dispatched.
	runtimeRelayWaitForCaptureStart(work)
	runtimeRelayNoCommand(t, commands)
	if _, published := promptOutcomeFacts(relay); published {
		t.Fatal("published early")
	}

	runtimeRelayWriteFile(t, filepath.Join(work, "release"), nil)
	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-second", "second")
	facts, published := promptOutcomeFacts(relay)
	if !published {
		t.Fatal("the outcome is published")
	}
	if facts == nil {
		t.Fatal("with the facts")
	}
	tree := facts.Worktree.Known()
	if tree == nil {
		t.Fatal("the working tree was read")
	}
	if !slices.ContainsFunc(tree.Shown, func(path core.ChangedPath) bool { return path.Path == "notes.txt" }) {
		t.Fatalf("shown = %#v", tree.Shown)
	}
	if facts.Processes.Known() == nil {
		t.Fatalf("%#v", facts.Processes)
	}
	events.close()
	runtimeRelayAwait(t, coordinator)
}

// The harness stopping while the observation runs ends it: the outcome is
// published with the facts unavailable and the reason, and the event that
// stopped it is recorded after the outcome.
// Hard-won: a09de965f follow-up: startup expiry's event drain overflowed the capture coordinator's stack.
func TestACaptureCancelledByTheHarnessStoppingPublishesUnavailableFacts(t *testing.T) {
	temp := t.TempDir()
	work := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "prompt-only", runtimeRelayTestsPrompt("only"))
	relay := NewRelayCell(durable)
	// A Git read that never ends: only the stop can end the capture.
	git := gatedGit(t, work, false)
	observer := captureObserver(relay, work, git)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator := runtimeRelaySpawnCapturingCoordinator(t, relay, eventRx, wakes, commands, observer)
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-only", "only")
	runtimeRelayEmit(t, events, core.RuntimeEventPromptFinished{
		Diagnostic: nil,
		RequestID:  "prompt-only",
		StopReason: "end_turn",
		Usage:      nil,
	})
	runtimeRelayWaitForCaptureStart(work)
	runtimeRelayEmit(t, events, core.RuntimeEventStopped{})
	runtimeRelayJoinOk(t, coordinator, "capturing coordinator")
	facts, published := promptOutcomeFacts(relay)
	if !published {
		t.Fatal("the outcome is published")
	}
	if facts == nil {
		t.Fatal("with facts that say they are unavailable")
	}
	if facts.Worktree.Known() != nil || !strings.Contains(facts.Worktree.Reason, "harness stopped") {
		t.Fatalf("%#v", facts)
	}
	if facts.Processes.Known() != nil {
		t.Fatalf("%#v", facts.Processes)
	}
}

// A Git read that never returns must not hold the turn's outcome, or the
// commands behind it: a cancellation accepted during the observation ends
// it, the outcome is published with the facts unavailable, and the Git read
// is killed with the dropped observation.
func TestACancellationAcceptedDuringACaptureEndsIt(t *testing.T) {
	temp := t.TempDir()
	work := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "prompt-only", runtimeRelayTestsPrompt("only"))
	relay := NewRelayCell(durable)
	git := gatedGit(t, work, false)
	observer := captureObserver(relay, work, git)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator := runtimeRelaySpawnCapturingCoordinator(t, relay, eventRx, wakes, commands, observer)
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-only", "only")
	runtimeRelayEmit(t, events, core.RuntimeEventPromptFinished{
		Diagnostic: nil,
		RequestID:  "prompt-only",
		StopReason: "end_turn",
		Usage:      nil,
	})
	runtimeRelayWaitForCaptureStart(work)
	if _, published := promptOutcomeFacts(relay); published {
		t.Fatal("published early")
	}

	runtimeRelaySubmitLocked(t, relay, "cancel-1", core.RelayCommandCancel{})
	runtimeRelayMust(t, wakes.trySend())
	runtimeRelayWaitForOutcome(relay)
	facts, _ := promptOutcomeFacts(relay)
	if facts == nil {
		t.Fatal("with facts")
	}
	if facts.Worktree.Known() != nil || !strings.Contains(facts.Worktree.Reason, "cancellation or close") {
		t.Fatalf("%#v", facts)
	}
	events.close()
	runtimeRelayAwait(t, coordinator)
}

// A cancellation accepted before the finishing event is drained is waiting
// for the same task that would observe the turn, and its wake may already be
// spent: the capture must see it on entry, not only when another wake comes.
func TestACancellationAcceptedBeforeACaptureStartsEndsIt(t *testing.T) {
	temp := t.TempDir()
	work := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "prompt-only", runtimeRelayTestsPrompt("only"))
	relay := NewRelayCell(durable)
	git := gatedGit(t, work, false)
	observer := captureObserver(relay, work, git)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator := runtimeRelaySpawnCapturingCoordinator(t, relay, eventRx, wakes, commands, observer)
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-only", "only")
	// Accepted with no wake sent, then the turn finishes: the cancellation is
	// still undelivered when the capture starts.
	runtimeRelaySubmitLocked(t, relay, "cancel-1", core.RelayCommandCancel{})
	runtimeRelayEmit(t, events, core.RuntimeEventPromptFinished{
		Diagnostic: nil,
		RequestID:  "prompt-only",
		StopReason: "end_turn",
		Usage:      nil,
	})
	runtimeRelayWaitForOutcome(relay)
	facts, _ := promptOutcomeFacts(relay)
	if facts == nil {
		t.Fatal("with facts")
	}
	if facts.Worktree.Known() != nil || !strings.Contains(facts.Worktree.Reason, "cancellation or close") {
		t.Fatalf("%#v", facts)
	}
	events.close()
	runtimeRelayAwait(t, coordinator)
}

// A session that is not a sub-agent has no observer, and its outcome says
// nothing of facts.
func TestAnOrdinarySessionsPromptOutcomeCarriesNoFacts(t *testing.T) {
	temp := t.TempDir()
	durable := runtimeRelayOpen(t, temp, "1.0.0")
	runtimeRelayTestsSubmit(t, durable, "prompt-only", runtimeRelayTestsPrompt("only"))
	relay := NewRelayCell(durable)
	events, eventRx := runtimeEventChannel()
	wakes := newDispatchWakeChannel()
	commands := newCommandChannel(4)
	coordinator, stop := runtimeRelaySpawnCoordinator(relay, eventRx, wakes, commands)
	defer stop()
	runtimeRelayEmit(t, events, core.RuntimeEventSessionConfigured{ConfigOptions: []acpschema.SessionConfigOption{}})
	assertPrompt(t, nextCommand(t, commands.receiver()), "prompt-only", "only")
	runtimeRelayEmit(t, events, core.RuntimeEventPromptFinished{
		Diagnostic: nil,
		RequestID:  "prompt-only",
		StopReason: "end_turn",
		Usage:      nil,
	})
	waitForRelayState(t, relay, func(state core.RelayOperationalState) bool { return state.ActivePrompt == nil })
	if facts, published := promptOutcomeFacts(relay); !published || facts != nil {
		t.Fatalf("facts = %#v, published = %v", facts, published)
	}
	events.close()
	runtimeRelayAwait(t, coordinator)
}

// runtimeRelayAwait waits for a task to stop and ignores its result (Rust:
// `let _ = task.await`).
func runtimeRelayAwait[T any](t *testing.T, task *core.JoinHandle[T]) {
	t.Helper()
	select {
	case <-task.Done():
	case <-time.After(ioTimeout):
		t.Fatal("the task did not stop")
	}
}

func TestPushGateStartupAndEnvironmentFollowHarnessAndPrimaryOwnership(t *testing.T) {
	for _, harness := range []core.HarnessKind{core.HarnessKindCodex, core.HarnessKindClaude, core.HarnessKindMuse} {
		temp := t.TempDir()
		repository := gitRepository(t, temp)
		launch := launchConfig("/test/profile")
		launch.Cwd = repository
		launch.Harness = harness
		launch.ReviewTrigger = core.ReviewTriggerPush
		launch.Environment["MJ_REVIEW_GATE"] = "inherited"
		notices, err := prepareReviewGate(&launch)
		runtimeRelayMust(t, err)
		spec := primarySupervisorSpec(&launch)
		if harness == core.HarnessKindMuse {
			if want := []string{"Before-push review is not available for Muse Code; this session runs without review."}; !slices.Equal(notices, want) {
				t.Fatalf("notices = %q", notices)
			}
			if runtimeRelayExists(filepath.Join(repository, ".git/hooks/pre-push")) {
				t.Fatal("Muse installed the push gate")
			}
			if _, ok := spec.Environment["MJ_REVIEW_GATE"]; ok {
				t.Fatal("Muse's bridge carries MJ_REVIEW_GATE")
			}
			if !slices.Contains(spec.ExcludedEnvironment, "MJ_REVIEW_GATE") {
				t.Fatalf("excluded = %q", spec.ExcludedEnvironment)
			}
		} else {
			if len(notices) != 0 {
				t.Fatalf("%v: notices = %q", harness, notices)
			}
			if hook := string(runtimeRelayReadFile(t, filepath.Join(repository, ".git/hooks/pre-push"))); !strings.Contains(hook, "# mj-review-gate") {
				t.Fatalf("pre-push = %q", hook)
			}
			if got := spec.Environment["MJ_REVIEW_GATE"]; got != "1" {
				t.Fatalf("MJ_REVIEW_GATE = %q", got)
			}
		}
		// The controller writes Off for workers that borrow the workspace.
		launch.ReviewTrigger = core.ReviewTriggerOff
		launch.Environment[core.SessionManagedSubagentEnv] = "1"
		notices, err = prepareReviewGate(&launch)
		runtimeRelayMust(t, err)
		if len(notices) != 0 {
			t.Fatalf("%v: managed child notices = %q", harness, notices)
		}
		if _, ok := primarySupervisorSpec(&launch).Environment["MJ_REVIEW_GATE"]; ok {
			t.Fatalf("%v: a managed child's bridge carries MJ_REVIEW_GATE", harness)
		}
		delete(launch.Environment, core.SessionManagedSubagentEnv)
		launch.ReviewTrigger = core.ReviewTriggerOff
		if _, ok := primarySupervisorSpec(&launch).Environment["MJ_REVIEW_GATE"]; ok {
			t.Fatalf("%v: a session without push review carries MJ_REVIEW_GATE", harness)
		}
	}
}

func TestAnOwningOrchestratorPinsTheStartAndInstallsThePushGate(t *testing.T) {
	temp := t.TempDir()
	repository := gitRepository(t, temp)
	launch := launchConfig("/test/profile")
	launch.Cwd = repository
	launch.SessionID = "review-child"
	launch.Harness = core.HarnessKindCodex
	launch.ReviewTrigger = core.ReviewTriggerPush
	launch.Environment[core.SessionManagedSubagentEnv] = "1"
	start := testSupportGit(repository, "rev-parse", "HEAD")
	if !launch.PreparesReview() || !launch.ReviewGate() {
		t.Fatalf("prepares review = %v, review gate = %v", launch.PreparesReview(), launch.ReviewGate())
	}
	notices, err := prepareReviewGate(&launch)
	runtimeRelayMust(t, err)
	if len(notices) != 0 {
		t.Fatalf("notices = %q", notices)
	}
	if !runtimeRelayExists(filepath.Join(repository, ".git/hooks/pre-push")) {
		t.Fatal("an owning orchestrator did not install the push gate")
	}
	if _, ok := primarySupervisorSpec(&launch).Environment["MJ_REVIEW_GATE"]; !ok {
		t.Fatal("an owning orchestrator's bridge lacks MJ_REVIEW_GATE")
	}
	runtimeRelayWriteFile(t, filepath.Join(repository, "tracked.rs"), []byte("fn main() { work(); }\n"))
	testSupportGit(repository, "commit", "-qam", "child work")
	head, err := workerreview.ReviewStartHead(checkpoint.SystemGit{}, repository, "review-child")
	runtimeRelayMust(t, err)
	if !runtimeRelayIs(head, start) {
		t.Fatalf("the first review covers the commits made after the child started: %v, want %q", head, start)
	}
	// A harness that cannot offer the tools records nothing and says nothing.
	launch.SessionID = "muse-child"
	launch.Harness = core.HarnessKindMuse
	launch.ReviewTrigger = core.ReviewTriggerOff
	if launch.PreparesReview() {
		t.Fatal("Muse prepares review")
	}
	notices, err = prepareReviewGate(&launch)
	runtimeRelayMust(t, err)
	if len(notices) != 0 {
		t.Fatalf("notices = %q", notices)
	}
}

func TestPushGateStartupReportsARefusedHookInstallOnce(t *testing.T) {
	temp := t.TempDir()
	repository := gitRepository(t, temp)
	hooks := filepath.Join(repository, ".git/hooks")
	runtimeRelayWriteFile(t, filepath.Join(hooks, "pre-push"), []byte("existing hook"))
	runtimeRelayWriteFile(t, filepath.Join(hooks, "pre-push.before-mj"), []byte("existing backup"))
	launch := launchConfig("/test/profile")
	launch.Cwd = repository
	launch.ReviewTrigger = core.ReviewTriggerPush
	notices, err := prepareReviewGate(&launch)
	runtimeRelayMust(t, err)
	if len(notices) != 1 {
		t.Fatalf("notices = %q", notices)
	}
	if !strings.Contains(notices[0], filepath.Join(hooks, "pre-push.before-mj")) {
		t.Fatalf("%s", notices[0])
	}
	if !strings.Contains(notices[0], "the gate is not installed") {
		t.Fatalf("%s", notices[0])
	}
	if got := string(runtimeRelayReadFile(t, filepath.Join(hooks, "pre-push"))); got != "existing hook" {
		t.Fatalf("pre-push = %q", got)
	}
}

// The harness preparation joins an attach's state before the relay digests
// it. A preparation that moves on while the relay itself is unchanged must
// still bring the whole state back, or the controller keeps a stale one.
func TestAChangedPreparationIsNeverTakenForAKnownState(t *testing.T) {
	temp := t.TempDir()
	relay := runtimeRelayCell(t, temp, "test")
	attach := func(known *string) core.RelayRequestEnvelope {
		return core.RelayRequestEnvelope{
			RequestID:       "attach",
			ProtocolVersion: core.RelayProtocolVersion,
			Request: core.RelayRequestAttach{
				AfterOrdinal:     0,
				AfterDigest:      core.RelayEventGenesisDigest,
				KnownStateDigest: known,
			},
		}
	}
	preparing := core.HarnessPreparationPreparing{Step: "harness-profile", SinceMs: 1}
	ctx := context.Background()
	response, err := handleRequest(ctx, relay, attach(nil), preparing)
	runtimeRelayMust(t, err)
	attached, ok := runtimeRelayOk(t, response, "expected the whole state").(core.RelayResponsePayloadAttached)
	if !ok {
		t.Fatalf("expected the whole state: %#v", response.Body)
	}
	if !reflect.DeepEqual(attached.State.HarnessPreparation, core.HarnessPreparation(preparing)) {
		t.Fatalf("preparation = %#v", attached.State.HarnessPreparation)
	}
	if attached.StateDigest == nil {
		t.Fatal("the attach carries no state digest")
	}
	held := *attached.StateDigest

	response, err = handleRequest(ctx, relay, attach(&held), preparing)
	runtimeRelayMust(t, err)
	if _, ok := runtimeRelayOk(t, response, "an unchanged state").(core.RelayResponsePayloadAttachedWithKnownState); !ok {
		t.Fatalf("an unchanged state was resent: %#v", response.Body)
	}
	response, err = handleRequest(ctx, relay, attach(&held), core.HarnessPreparationStarted{})
	runtimeRelayMust(t, err)
	attached, ok = runtimeRelayOk(t, response, "a changed preparation must resend the state").(core.RelayResponsePayloadAttached)
	if !ok {
		t.Fatalf("a changed preparation must resend the state: %#v", response.Body)
	}
	if !reflect.DeepEqual(attached.State.HarnessPreparation, core.HarnessPreparation(core.HarnessPreparationStarted{})) {
		t.Fatalf("preparation = %#v", attached.State.HarnessPreparation)
	}
}

// runtimeRelayChangeWait is one finished waitForChange.
type runtimeRelayChangeWait struct {
	body core.RelayResponseBody
	err  error
}

// A change wait answers when the relay changes and not before, and with the
// values it was given once its bound passes without a change. Every relay
// change takes the relay lock, so the lock's release is what wakes it.
func TestAChangeWaitAnswersOnARelayChangeOrAtItsBound(t *testing.T) {
	synctest.Test(t, func(t *testing.T) {
		temp := t.TempDir()
		relay := runtimeRelayCell(t, temp, "test")
		feed := defaultSubagentFeed()
		ctx := context.Background()
		current := func() (uint64, string) {
			state := runtimeRelayOperationalState(relay)
			state.HarnessPreparation = nil
			digest, err := core.RelayStateDigest(&state)
			runtimeRelayMust(t, err)
			return state.LatestOrdinal, digest
		}
		observed := func(body core.RelayResponseBody, err error) (uint64, string, bool) {
			t.Helper()
			runtimeRelayMust(t, err)
			if ok, isOk := body.(core.RelayResponseBodyOk); isOk {
				if change, isChange := ok.Payload.(core.RelayResponsePayloadChangeObserved); isChange {
					return change.LatestOrdinal, change.StateDigest, change.SubagentQueueChanged
				}
			}
			t.Fatalf("expected an observed change, got %#v", body)
			return 0, "", false
		}
		wait := func(ordinal uint64, digest string) *core.JoinHandle[runtimeRelayChangeWait] {
			return core.Spawn(func() runtimeRelayChangeWait {
				// Each wait owns its connection's guardian generation.
				var guardianGeneration uint64
				body, err := waitForChange(ctx, relay, nil, nil, &feed, &guardianGeneration, ordinal, &digest)
				return runtimeRelayChangeWait{body: body, err: err}
			})
		}
		ordinal, digest := current()

		started := time.Now()
		body, err := waitForChange(ctx, relay, nil, nil, &feed, new(uint64), ordinal, &digest)
		latest, observedDigest, queueChanged := observed(body, err)
		if latest != ordinal || observedDigest != digest || queueChanged {
			t.Fatalf("quiet wait observed %d %s %v", latest, observedDigest, queueChanged)
		}
		if elapsed := time.Since(started); elapsed != core.RelayChangeWaitBound {
			t.Fatalf("quiet wait took %v", elapsed)
		}

		started = time.Now()
		changing := wait(ordinal, digest)
		time.Sleep(time.Second)
		{
			locked := relay.Lock()
			_, err := locked.RecordObservation(core.RelayObservationWarning{Message: "something happened"})
			locked.Unlock()
			runtimeRelayMust(t, err)
		}
		changed := runtimeRelayJoin(t, changing, "the change wait")
		latest, changedDigest, _ := observed(changed.body, changed.err)
		if latest != ordinal+1 {
			t.Fatalf("latest = %d, want %d", latest, ordinal+1)
		}
		if changedDigest == digest {
			t.Fatal("the state digest did not change")
		}
		if elapsed := time.Since(started); elapsed != time.Second {
			t.Fatalf("the change was answered after %v", elapsed)
		}

		body, err = waitForChange(ctx, relay, nil, nil, &feed, new(uint64), ordinal, &digest)
		if stale, _, _ := observed(body, err); stale != ordinal+1 {
			t.Fatalf("a stale view is answered at once: %d", stale)
		}

		// Waits only observe the relay, so two of them on an unchanged relay stay
		// idle until their bound instead of waking each other.
		ordinal, digest = current()
		started = time.Now()
		first := wait(ordinal, digest)
		second := wait(ordinal, digest)
		firstResult := runtimeRelayJoin(t, first, "the first wait")
		secondResult := runtimeRelayJoin(t, second, "the second wait")
		if got, _, _ := observed(firstResult.body, firstResult.err); got != ordinal {
			t.Fatalf("first wait observed %d", got)
		}
		if got, _, _ := observed(secondResult.body, secondResult.err); got != ordinal {
			t.Fatalf("second wait observed %d", got)
		}
		if elapsed := time.Since(started); elapsed != core.RelayChangeWaitBound {
			t.Fatalf("two waits took %v", elapsed)
		}
	})
}

func TestAntigravityConnectedWithHookPublishesSteeringAndEnablesAutomaticSteering(t *testing.T) {
	durable, err := DurableRelayOpen(t.TempDir(), runtimeRelayTestsSessionID, "1.0.0")
	runtimeRelayMust(t, err)
	relay := NewRelayCell(durable)
	guard := relay.Lock()
	setVerdictHarness(t, guard.DurableRelay, core.HarnessKindAntigravity)
	guard.SetSteeringDeliveryMode(core.HarnessKindAntigravity, true)
	guard.Unlock()
	agentName, agentVersion := "antigravity", "1.5.11"
	protocolVersion := acpschema.ProtocolVersion(1)
	unsupported := false
	_, err = recordRuntimeEvent(relay, map[string]core.RelayCommand{}, core.RuntimeEventConnected{
		AgentName:         &agentName,
		AgentVersion:      &agentVersion,
		ProtocolVersion:   &protocolVersion,
		Capabilities:      &acpschema.AgentCapabilities{},
		AgentInfo:         &acpschema.Implementation{Name: "antigravity", Version: "1.5.11"},
		SteeringSupported: &unsupported,
	})
	runtimeRelayMust(t, err)

	locked := relay.Lock()
	defer locked.Unlock()
	if supported := locked.OperationalState().SteeringSupported; supported == nil || !*supported {
		t.Fatalf("published steering support = %v, want true", supported)
	}
	// Verify automatic steering is enabled by starting a turn and queueing a
	// follow-up prompt.
	locked.setAcpReady(true)
	_, err = locked.RecordObservation(core.RelayObservationSessionOpened{NativeSessionID: "agy-conv-connected"})
	runtimeRelayMust(t, err)
	submit := func(id, text string) {
		t.Helper()
		response := locked.Handle(core.RelayRequestEnvelope{
			RequestID:       id,
			ProtocolVersion: core.RelayProtocolVersion,
			Request: core.RelayRequestSubmit{CommandID: id, Command: core.RelayCommandPrompt{
				Prompt: []acpschema.ContentBlock{relayTextBlock(text)},
			}},
		})
		if _, ok := response.Body.(core.RelayResponseBodyOk); !ok {
			t.Fatalf("%#v", response)
		}
	}
	submit("agy-connected-first", "first")
	claimed, err := locked.ClaimPendingCommands(true)
	runtimeRelayMust(t, err)
	if len(claimed) != 1 {
		t.Fatalf("claimed %d commands", len(claimed))
	}
	submit("agy-connected-steer", "steer me")
	claimed, err = locked.ClaimPendingCommands(true)
	runtimeRelayMust(t, err)
	if len(claimed) != 0 {
		t.Fatalf("a hook steer was claimed as an ACP command: %#v", claimed)
	}
	answer, err := locked.DrainMailbox("PostToolBatch", nil)
	runtimeRelayMust(t, err)
	drained, ok := answer.(core.RelayResponsePayloadMailboxDrained)
	if !ok || drained.LeaseID == nil || drained.Text == nil || drained.Count != 1 || !strings.Contains(*drained.Text, "steer me") {
		t.Fatalf("drain = %#v", answer)
	}
}

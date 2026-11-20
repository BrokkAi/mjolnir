//go:build unix

// Port of mj-worker/src/worker_runtime/relay_tests.rs.
//
// The relay tests of the worker runtime: they drive the relay coordinator,
// the control socket and the worker daemon over real sockets, files and
// processes. Rust compiles the module under cfg(all(test, unix)).
//
// Helpers whose Rust names other mj-worker modules also declare carry the
// prefix `runtimeRelayTests` (SESSION_ID, prompt, submit, agent_output and
// run_relay_coordinator); relay/tests.rs uses `relayTests`.

package worker

import (
	"bufio"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// runtimeRelayTestsSessionID is the session every fixture in this module
// opens (Rust's SESSION_ID).
const runtimeRelayTestsSessionID = "018f9dd2-a3b4-7c8d-9000-123456789abc"

// delegationExchange sends one relay request over a served connection and
// returns the payload of its successful response.
func delegationExchange(t *testing.T, lines *bufio.Reader, writer io.Writer, request core.RelayRequest) core.RelayResponsePayload {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs delegation_exchange")
}

func projectMemoryTransferExchange(t *testing.T, lines *bufio.Reader, writer io.Writer, requestID string, action core.ProjectMemoryTransferAction) core.RelayResponseEnvelope {
	t.Helper()
	return projectMemoryTransferExchangeAt(t, lines, writer, requestID, core.RelayProtocolVersion, action)
}

func projectMemoryTransferExchangeAt(t *testing.T, lines *bufio.Reader, writer io.Writer, requestID string, protocolVersion uint32, action core.ProjectMemoryTransferAction) core.RelayResponseEnvelope {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs project_memory_transfer_exchange_at")
}

func nextProjectMemoryTransferExchange(t *testing.T, lines *bufio.Reader, writer io.Writer, sequence *uint64, action core.ProjectMemoryTransferAction) core.RelayResponseEnvelope {
	t.Helper()
	requestID := fmt.Sprintf("project-memory-transfer-%d", *sequence)
	*sequence++
	return projectMemoryTransferExchange(t, lines, writer, requestID, action)
}

func uploadProjectMemoryBytes(t *testing.T, lines *bufio.Reader, writer io.Writer, sequence *uint64, bytes []byte, kind core.ProjectMemoryUploadKind) core.RelayResponsePayload {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs upload_project_memory_bytes")
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
func projectMemorySocket(t *testing.T, relay *RelayCell, endpoint ProjectMemoryEndpoint) (net.Conn, *core.JoinHandle[error]) {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs project_memory_socket")
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

func runtimeEventChannel() (TestRuntimeEventSender, <-chan core.RuntimeEvent) {
	events := make(chan core.RuntimeEvent, acpEventChannelCapacity)
	return TestRuntimeEventSender{sender: events}, events
}

// runtimeRelayTestsRunRelayCoordinator runs the relay coordinator for a
// protocol fixture (Rust's run_relay_coordinator). Protocol fixtures have no
// classifier. Verdict fixtures supply their own local HTTP endpoint instead
// of resolving the host's default service.
func runtimeRelayTestsRunRelayCoordinator(ctx context.Context, relay *RelayCell, events <-chan core.RuntimeEvent, wakes <-chan struct{}, commands chan<- CommandRequest) error {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs run_relay_coordinator")
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
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs expect_daemon_launch_failure")
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
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs git_repository")
}

// testCredentials is the credential endpoint of launchConfig("/profile").
// The error is Rust's Err(String), which the connection reports.
func testCredentials() (CredentialEndpoint, error) {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs test_credentials")
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
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs credential_request")
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

// runtimeRelayTestsPrompt is a prompt command of one text block (Rust's
// prompt).
func runtimeRelayTestsPrompt(text string) core.RelayCommand {
	return core.RelayCommandPrompt{Prompt: []acpschema.ContentBlock{{
		Kind: acpschema.ContentBlockKindText,
		Text: &acpschema.TextContent{Text: text},
	}}}
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

// elicitationRequest is an out-of-band ACP command: it shares the dispatch
// channel but never reaches durable relay state.
func elicitationRequest() CommandRequest {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs elicitation_request")
}

// recordedWarning is whether a runtime warning reached durable state. A
// coordinator that parked on a command send stops draining runtime events,
// so this stays false forever once that happens.
func recordedWarning(relay *RelayCell, message string) bool {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs recorded_warning")
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
		t.Fatal("expected ACP prompt command")
	}
	if prompt.RequestID != expectedID {
		t.Fatalf("request ID = %q, want %q", prompt.RequestID, expectedID)
	}
	if len(prompt.Prompt) != 1 || prompt.Prompt[0].Kind != acpschema.ContentBlockKindText ||
		prompt.Prompt[0].Text == nil || prompt.Prompt[0].Text.Text != expectedText {
		t.Fatalf("prompt = %#v, want one text block %q", prompt.Prompt, expectedText)
	}
}

func waitForRelayState(t *testing.T, relay *RelayCell, predicate func(core.RelayOperationalState) bool) {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs wait_for_relay_state")
}

// runtimeRelayTestsAgentOutput is an agent message chunk as the ACP
// runtime forwards it (Rust's agent_output).
func runtimeRelayTestsAgentOutput(text, messageID string) core.RuntimeEvent {
	id := acpschema.MessageId(messageID)
	update, err := json.Marshal(acpschema.SessionUpdate{
		Kind: acpschema.SessionUpdateKindAgentMessageChunk,
		AgentMessageChunk: &acpschema.ContentChunk{
			Content:   acpschema.ContentBlock{Kind: acpschema.ContentBlockKindText, Text: &acpschema.TextContent{Text: text}},
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
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs harness_turn_open")
}

// pagedRelayHistory is a history that seals several journal segments and
// overflows one replay page, so an attach against it really reads and
// decompresses from disk.
func pagedRelayHistory(t *testing.T, root string, events int) *DurableRelay {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs paged_relay_history")
}

func attachFrame(requestID string, afterOrdinal uint64, afterDigest string) []byte {
	encoded, err := json.Marshal(core.RelayRequestEnvelope{
		RequestID:       requestID,
		ProtocolVersion: core.RelayProtocolVersion,
		Request:         core.RelayRequestAttach{AfterOrdinal: afterOrdinal, AfterDigest: afterDigest},
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
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs LiveRecorder::start")
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
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs repository_whose_staging_blocks")
}

// workerStatus asks the worker serving root's control socket for its
// operational state.
func workerStatus(ctx context.Context, root string) (core.RelayOperationalState, error) {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs worker_status")
}

// startupStepRecorded is whether the worker's startup record already names
// step.
//
// The record outlives the worker, which makes it the reliable witness for a
// step whose visible effect can vanish first: a worker whose harness exits
// at once removes `control.sock` again during teardown.
func startupStepRecorded(root, step string) bool {
	bytes, err := os.ReadFile(filepath.Join(root, core.WorkerStartupFile))
	if err != nil {
		return false
	}
	var record any
	if json.Unmarshal(bytes, &record) != nil {
		return false
	}
	object, _ := record.(map[string]any)
	steps, _ := object["steps"].([]any)
	for _, recorded := range steps {
		entry, _ := recorded.(map[string]any)
		if name, ok := entry["step"].(string); ok && name == step {
			return true
		}
	}
	return false
}

// coordinatorBehindAReadyBarrier starts a coordinator whose session has a
// ready checkpoint barrier, fed by a runtime event channel of capacity.
// Returns the cut the barrier holds.
func coordinatorBehindAReadyBarrier(t *testing.T, temp string, capacity int) (*RelayCell, chan<- core.RuntimeEvent, chan<- struct{}, *core.JoinHandle[error], core.RelayCursor) {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs coordinator_behind_a_ready_barrier")
}

func journaledNotices(relay *RelayCell) []string {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs journaled_notices")
}

// runCapturingCoordinator is a sub-agent's coordinator: it observes its
// agent's work when a prompt ends.
func runCapturingCoordinator(ctx context.Context, relay *RelayCell, events <-chan core.RuntimeEvent, wakes <-chan struct{}, commands chan<- CommandRequest, observer WorkObserver) error {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs run_capturing_coordinator")
}

// promptOutcomeFacts is the facts a prompt's published outcome carries, once
// it is published: published false is Rust's None, and a nil facts with
// published true is an outcome without any (Some(None)).
func promptOutcomeFacts(relay *RelayCell) (facts *core.WorkFacts, published bool) {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs prompt_outcome_facts")
}

// gatedGit is a fake `git` whose `status` announces itself and then waits
// for a release file (or forever when release is false), so a test can hold
// the capture open.
func gatedGit(t *testing.T, directory string, release bool) string {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs gated_git")
}

func captureObserver(relay *RelayCell, directory string, git string) WorkObserver {
	panic("unported: mj-worker/src/worker_runtime/relay_tests.rs capture_observer")
}

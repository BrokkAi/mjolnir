// Port of the test-only helpers of mj-worker/src/acp/drive.rs, and the
// shared fixtures of the drive tests (plan_tests.rs, claude_result_tests.rs,
// session_directories_tests.rs): tokio's in-memory duplex stream, a default
// launch, and a probe that plays the agent's side of one ACP connection.

package worker

import (
	"bufio"
	"context"
	"encoding/json"
	"io"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// drive runs one bridge's protocol over transport. Tests own their
// connection registry here; production holds it through process teardown.
func drive(ctx context.Context, transport ByteStreams, spec LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent, opened *Shared[*OpenedSession], replacingPreviousBridge bool) (SessionRestart, error) {
	var firstReplacedUnusedNativeSessionID *string
	return driveBridge(ctx, transport, spec, requests, events, opened, BridgeSessionOpen{
		replacingPreviousBridge:            replacingPreviousBridge,
		replaceUnusedCodexThread:           shouldReplaceUnusedCodexThread(&spec),
		firstReplacedUnusedNativeSessionID: &firstReplacedUnusedNativeSessionID,
	})
}

func driveBridge(ctx context.Context, transport ByteStreams, spec LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent, opened *Shared[*OpenedSession], sessionOpen BridgeSessionOpen) (SessionRestart, error) {
	guardian := guardianForLaunch(&spec, nil)
	terminals, err := NewTerminalRegistry()
	if err != nil {
		return nil, err
	}
	restart, err := driveWithTerminals(ctx, transport, spec, requests, events, opened, sessionOpen,
		BridgeOwned{terminals: terminals, guardian: guardian})
	if guardian != nil {
		guardian.shutdown(context.WithoutCancel(ctx))
	}
	return restart, err
}

// acpDrivePipe is one direction of an in-memory duplex stream: a buffer of
// at most capacity bytes, whose writer waits for room and whose reader waits
// for data.
type acpDrivePipe struct {
	mu           sync.Mutex
	changed      *sync.Cond
	buffer       []byte
	capacity     int
	writerClosed bool
	readerClosed bool
}

func newACPDrivePipe(capacity int) *acpDrivePipe {
	pipe := &acpDrivePipe{capacity: capacity}
	pipe.changed = sync.NewCond(&pipe.mu)
	return pipe
}

func (p *acpDrivePipe) read(data []byte) (int, error) {
	p.mu.Lock()
	defer p.mu.Unlock()
	for len(p.buffer) == 0 && !p.writerClosed && !p.readerClosed {
		p.changed.Wait()
	}
	if p.readerClosed {
		return 0, io.ErrClosedPipe
	}
	if len(p.buffer) == 0 {
		return 0, io.EOF
	}
	n := copy(data, p.buffer)
	p.buffer = p.buffer[n:]
	p.changed.Broadcast()
	return n, nil
}

func (p *acpDrivePipe) write(data []byte) (int, error) {
	p.mu.Lock()
	defer p.mu.Unlock()
	written := 0
	for written < len(data) {
		for len(p.buffer) >= p.capacity && !p.readerClosed && !p.writerClosed {
			p.changed.Wait()
		}
		if p.readerClosed || p.writerClosed {
			return written, io.ErrClosedPipe
		}
		room := min(p.capacity-len(p.buffer), len(data)-written)
		p.buffer = append(p.buffer, data[written:written+room]...)
		written += room
		p.changed.Broadcast()
	}
	return written, nil
}

func (p *acpDrivePipe) closeWriter() {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.writerClosed = true
	p.changed.Broadcast()
}

func (p *acpDrivePipe) closeReader() {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.readerClosed = true
	p.changed.Broadcast()
}

// acpDriveStream is one end of tokio::io::duplex: it reads what the other
// end writes. Closing it closes both directions, as dropping a DuplexStream
// does.
type acpDriveStream struct {
	in  *acpDrivePipe
	out *acpDrivePipe
}

func (s *acpDriveStream) Read(data []byte) (int, error)  { return s.in.read(data) }
func (s *acpDriveStream) Write(data []byte) (int, error) { return s.out.write(data) }
func (s *acpDriveStream) Close() error {
	s.in.closeReader()
	s.out.closeWriter()
	return nil
}

// readHalf and writeHalf are tokio::io::split's halves: closing one closes
// only its own direction.
func (s *acpDriveStream) readHalf() io.ReadCloser   { return acpDriveReadHalf{s} }
func (s *acpDriveStream) writeHalf() io.WriteCloser { return acpDriveWriteHalf{s} }

type acpDriveReadHalf struct{ stream *acpDriveStream }

func (h acpDriveReadHalf) Read(data []byte) (int, error) { return h.stream.in.read(data) }
func (h acpDriveReadHalf) Close() error {
	h.stream.in.closeReader()
	return nil
}

type acpDriveWriteHalf struct{ stream *acpDriveStream }

func (h acpDriveWriteHalf) Write(data []byte) (int, error) { return h.stream.out.write(data) }
func (h acpDriveWriteHalf) Close() error {
	h.stream.out.closeWriter()
	return nil
}

// acpDriveDuplex is tokio::io::duplex(capacity).
func acpDriveDuplex(capacity int) (*acpDriveStream, *acpDriveStream) {
	forward, backward := newACPDrivePipe(capacity), newACPDrivePipe(capacity)
	return &acpDriveStream{in: backward, out: forward}, &acpDriveStream{in: forward, out: backward}
}

// clientTransport is the client side of a duplex as the runtime's
// transport.
func (s *acpDriveStream) clientTransport() ByteStreams {
	return NewByteStreams(s.writeHalf(), s.readHalf())
}

// acpDriveSpec is the LaunchSpec the drive tests start from: a native
// subagent policy, no memory, no relay owner, classification off and no
// stall bound but the one from the environment.
func acpDriveSpec(command string, harness core.HarnessKind, policy core.ExecutionPolicy, cwd string) LaunchSpec {
	return LaunchSpec{
		LoginIdentity:         &ClaudeLoginIdentity{},
		OperatorAway:          &OperatorAway{},
		GoalRecovery:          NewShared(core.GoalRecoveryContext{}),
		Command:               command,
		Args:                  []string{},
		Environment:           map[string]string{},
		Cwd:                   cwd,
		AdditionalDirectories: []string{},
		SubagentPolicy:        core.SubagentPolicy{Mode: core.SubagentPolicyNative},
		NativeSessionHistory:  NativeSessionHistoryLoadBeforeReplace,
		AcceptedConfig:        NewShared(core.AcceptedSessionConfig{}),
		Harness:               harness,
		ExecutionPolicy:       policy,
		ACPActivity:           core.NewAcpActivityClock(),
		StepClock:             &core.StepClock{},
		ToolsInFlight:         &core.ToolsInFlight{},
		TurnContext:           &transcript.TurnContext{},
		Verdict:               VerdictSourceDisabled{},
	}
}

// acpProbe plays the agent's side of one ACP connection the runtime drives,
// in the test's hands: a real ACP transport with no fake behind it.
type acpProbe struct {
	t            *testing.T
	operatorAway *OperatorAway
	input        chan string
	inputClosed  chan struct{}
	output       io.WriteCloser
	agentStream  *acpDriveStream
	commands     chan CommandRequest
	events       chan core.RuntimeEvent
	driverDone   chan acpProbeOutcome
	cancel       context.CancelFunc
}

type acpProbeOutcome struct {
	restart SessionRestart
	err     error
}

// startACPProbe drives spec over a duplex of capacity bytes and returns the
// probe holding the agent's side.
func startACPProbe(t *testing.T, spec LaunchSpec, capacity int) *acpProbe {
	t.Helper()
	client, agent := acpDriveDuplex(capacity)
	ctx, cancel := context.WithCancel(context.Background())
	probe := &acpProbe{
		t:            t,
		operatorAway: spec.OperatorAway,
		input:        make(chan string, 1024),
		inputClosed:  make(chan struct{}),
		output:       agent.writeHalf(),
		agentStream:  agent,
		commands:     make(chan CommandRequest, 16),
		events:       make(chan core.RuntimeEvent, 128),
		driverDone:   make(chan acpProbeOutcome, 1),
		cancel:       cancel,
	}
	go func() {
		defer close(probe.inputClosed)
		lines := bufio.NewReaderSize(agent.readHalf(), 64*1024)
		for {
			line, err := lines.ReadString('\n')
			if line != "" {
				probe.input <- strings.TrimRight(line, "\r\n")
			}
			if err != nil {
				return
			}
		}
	}()
	go func() {
		restart, err := drive(ctx, client.clientTransport(), spec, probe.commands, probe.events, NewShared[*OpenedSession](nil), false)
		probe.driverDone <- acpProbeOutcome{restart: restart, err: err}
	}()
	// Rust's Drop aborts the driver.
	t.Cleanup(func() {
		cancel()
		agent.Close()
	})
	return probe
}

// message is the next frame the runtime sent, decoded.
func (p *acpProbe) message() map[string]any {
	p.t.Helper()
	select {
	case line := <-p.input:
		return p.decodeMessage(line)
	case <-p.inputClosed:
		select {
		case line := <-p.input:
			return p.decodeMessage(line)
		default:
		}
		p.t.Fatal("ACP closed unexpectedly")
	case <-time.After(5 * time.Second):
		p.t.Fatal("ACP message timed out")
	}
	return nil
}

// decodeMessage decodes one frame, keeping numbers as their JSON text so a
// request id is answered exactly as it was sent.
func (p *acpProbe) decodeMessage(line string) map[string]any {
	p.t.Helper()
	decoder := json.NewDecoder(strings.NewReader(line))
	decoder.UseNumber()
	var message map[string]any
	if err := decoder.Decode(&message); err != nil {
		p.t.Fatalf("decode ACP message %q: %v", line, err)
	}
	return message
}

// noMessage asserts nothing reaches the agent for a moment.
func (p *acpProbe) noMessage(wait time.Duration) {
	p.t.Helper()
	select {
	case line := <-p.input:
		p.t.Fatalf("unexpected ACP message to the agent: %s", line)
	case <-time.After(wait):
	}
}

func (p *acpProbe) send(message any) {
	p.t.Helper()
	encoded, err := jsonvalue.Marshal(message)
	if err != nil {
		p.t.Fatalf("encode %v: %v", message, err)
	}
	if _, err := p.output.Write(append(encoded, '\n')); err != nil {
		p.t.Fatalf("write to the runtime: %v", err)
	}
}

func (p *acpProbe) result(request map[string]any, result any) {
	p.t.Helper()
	p.send(map[string]any{"jsonrpc": "2.0", "id": request["id"], "result": result})
}

// refuse answers a mode request the way Claude Code refuses Auto.
func (p *acpProbe) refuse(request map[string]any) {
	p.t.Helper()
	p.send(map[string]any{"jsonrpc": "2.0", "id": request["id"], "error": map[string]any{
		"code": -32603, "message": "Internal error",
		"data": map[string]any{"details": "Cannot set permission mode: this bridge refuses it"},
	}})
}

func (p *acpProbe) event() core.RuntimeEvent {
	p.t.Helper()
	select {
	case event := <-p.events:
		return event
	case <-time.After(5 * time.Second):
		p.t.Fatal("runtime event timed out")
	}
	return nil
}

// command sends the runtime a command.
func (p *acpProbe) command(request CommandRequest) {
	p.t.Helper()
	select {
	case p.commands <- request:
	case <-time.After(5 * time.Second):
		p.t.Fatal("the runtime takes no commands")
	}
}

// resolve answers elicitation id and returns the runtime's verdict on the
// answer.
func (p *acpProbe) resolve(id string, response core.ElicitationResponse) error {
	p.t.Helper()
	resolved := make(chan error, 1)
	p.command(CommandRequestResolveElicitation{ElicitationID: id, Response: response, Resolved: resolved})
	select {
	case outcome := <-resolved:
		return outcome
	case <-time.After(5 * time.Second):
		p.t.Fatal("the elicitation answer was not resolved")
	}
	return nil
}

// noFinish drains events for a moment and requires that no prompt
// finishes.
func (p *acpProbe) noFinish(wait time.Duration) {
	p.t.Helper()
	deadline := time.After(wait)
	for {
		select {
		case event := <-p.events:
			if _, finished := event.(core.RuntimeEventPromptFinished); finished {
				p.t.Fatalf("no prompt may finish yet: %#v", event)
			}
		case <-deadline:
			return
		}
	}
}

// outcome waits for the runtime to stop and returns what it stopped with.
func (p *acpProbe) outcome() (SessionRestart, error) {
	p.t.Helper()
	select {
	case outcome := <-p.driverDone:
		return outcome.restart, outcome.err
	case <-time.After(5 * time.Second):
		p.t.Fatal("the runtime did not stop")
	}
	return nil, nil
}

// jsonString is a decoded JSON member as a string, empty when it is not
// one.
func jsonString(value any) string {
	text, _ := value.(string)
	return text
}

// jsonAt is the member under keys, through nested decoded objects.
func jsonAt(value any, keys ...string) any {
	for _, key := range keys {
		object, ok := value.(map[string]any)
		if !ok {
			return nil
		}
		value = object[key]
	}
	return value
}

// decodeJSONValue decodes text as a generic JSON value (Rust's
// serde_json::Value as the tests read it).
func decodeJSONValue(t *testing.T, text []byte) any {
	t.Helper()
	var value any
	if err := json.Unmarshal(text, &value); err != nil {
		t.Fatalf("decode %s: %v", text, err)
	}
	return value
}

// jsonEqual compares two generic JSON values as serde_json's Value equality
// does: object members by key, whatever their order.
func jsonEqual(a, b any) bool {
	left, err := json.Marshal(a)
	if err != nil {
		return false
	}
	right, err := json.Marshal(b)
	if err != nil {
		return false
	}
	var x, y any
	if json.Unmarshal(left, &x) != nil || json.Unmarshal(right, &y) != nil {
		return false
	}
	return reflect.DeepEqual(x, y)
}

// textPrompt is a prompt of one text block.
func textPrompt(text string) []schema.ContentBlock {
	return []schema.ContentBlock{{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: text}}}
}

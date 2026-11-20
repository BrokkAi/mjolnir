// Port of mj-worker/src/acp.rs.

// ACP runtime and normalized session controls used by a Hel session worker.
//
// The worker owns exactly one harness process and one foreground session. It
// deliberately does not know about orchestration, turn reviews, or subagents;
// core's ACP surface projects protocol capabilities for the chat control
// surface.

package worker

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"sync"
	"sync/atomic"
	"time"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Shared is a value several owners share behind one lock: Rust's
// Arc<Mutex<T>>. Share it by pointer; the zero value holds T's zero value.
type Shared[T any] struct {
	mu    sync.Mutex
	value T
}

// NewShared is Arc::new(Mutex::new(value)).
func NewShared[T any](value T) *Shared[T] {
	return &Shared[T]{value: value}
}

// Lock locks the value and returns it with the function that unlocks it,
// as Rust's lock guard does when it drops.
func (s *Shared[T]) Lock() (*T, func()) {
	s.mu.Lock()
	return &s.value, s.mu.Unlock
}

// ByteStreams is the byte streams one ACP connection runs over, as the Rust
// SDK's ByteStreams transport holds them: what this side writes (the
// bridge's stdin, or a test's in-memory pipe) and what it reads (the
// bridge's stdout).
type ByteStreams struct {
	Outgoing io.WriteCloser
	Incoming io.ReadCloser
}

// NewByteStreams is ByteStreams::new(outgoing, incoming).
func NewByteStreams(outgoing io.WriteCloser, incoming io.ReadCloser) ByteStreams {
	return ByteStreams{Outgoing: outgoing, Incoming: incoming}
}

// JSON-RPC's standard error codes, as agent_client_protocol::ErrorCode names
// them; acp-go exports only the ACP-specific ones.
const (
	rpcCodeParseError     = -32700
	rpcCodeInvalidRequest = -32600
	rpcCodeMethodNotFound = -32601
	rpcCodeInvalidParams  = -32602
	rpcCodeInternalError  = -32603
)

// rpcErrorWithData is agent_client_protocol::Error::new(code,
// message).data(Value::String(data)).
func rpcErrorWithData(code int, message, data string) *acp.RPCError {
	encoded, err := json.Marshal(data)
	if err != nil {
		panic(fmt.Sprintf("encode a JSON string: %v", err))
	}
	return &acp.RPCError{Code: code, Message: message, Data: encoded}
}

// acpErrorText is the Display of the Rust SDK's ACP error, which messages
// show: its message (its code when the message is empty), then ": " and its
// data pretty-printed as serde_json::to_string_pretty writes it, when it has
// any.
func acpErrorText(err *acp.RPCError) string {
	panic("unported: mj-worker/src/acp.rs acp_error_text")
}

// acpDisplayError wraps an ACP RPC error so that its text, alone and inside
// a wrapping chain, is the Rust SDK's Display (acpErrorText), as anyhow
// chains show it in Rust. errors.As still finds the *acp.RPCError.
type acpDisplayError struct {
	err *acp.RPCError
}

func (e acpDisplayError) Error() string { return acpErrorText(e.err) }

func (e acpDisplayError) Unwrap() error { return e.err }

// acpErrorDisplay is how Rust shows an error the ACP SDK returned:
// acpErrorText for an RPC error, otherwise the error's own text (a
// connection that ended, for example).
func acpErrorDisplay(err error) string {
	var rpc *acp.RPCError
	if errors.As(err, &rpc) {
		return acpErrorText(rpc)
	}
	return err.Error()
}

// bridgeStderrContext heads the context a failed bridge's stderr tail is
// attached as.
const bridgeStderrContext = "ACP bridge stderr:"

// WorkerExitReason is what a worker's exit record says it stopped on: the
// error chain on one line, then the bridge's stderr tail when there is one.
//
// The tail is the outermost context of a bridge failure, so the plain chain
// began with "ACP bridge stderr:" and dozens of bridge log lines, and the
// cause came last. A resume that failed this way stored the whole dump as the
// session's error (R8-2). With the cause first, the record's first line says
// why the worker stopped.
func WorkerExitReason(err error) string {
	panic("unported: mj-worker/src/acp.rs worker_exit_reason")
}

// hiddenContextBlock is the first block of a prompt the relay attached
// controller-only context to (project memory, shell results, a hand-off): an
// embedded resource named core.HiddenPromptContextURI. promptForHarness
// decides the form the harness's bridge receives.
func hiddenContextBlock(text string) schema.ContentBlock {
	return schema.ContentBlock{
		Kind: schema.ContentBlockKindResource,
		Resource: &schema.EmbeddedResource{
			Resource: schema.EmbeddedResourceResource{
				TextResourceContents: &schema.TextResourceContents{Text: text, URI: core.HiddenPromptContextURI},
			},
		},
	}
}

// promptForHarness is a prompt as the harness's bridge receives it.
//
// Codex receives the hidden context as the embedded resource. codex-acp
// titles a new thread from the text blocks of its first prompt alone; given
// the context as the first text block, it named sessions after Mjolnir's
// project memory ("Project memory instructions") instead of the user's
// request (launch finding I2-1). The model still reads the resource, which
// codex-acp passes on as `{uri}\n<context ref="{uri}">\n{text}\n</context>`.
//
// Every other harness receives the context as the text block it always has.
// Only Codex was seen to title a session after the memory, and ACP lets a
// client send embedded resources only to an agent that advertises them.
func promptForHarness(harness core.HarnessKind, prompt []schema.ContentBlock) []schema.ContentBlock {
	if harness == core.HarnessKindCodex {
		return prompt
	}
	blocks := make([]schema.ContentBlock, 0, len(prompt))
	for _, block := range prompt {
		if block.Resource != nil {
			if resource := block.Resource.Resource.TextResourceContents; resource != nil && resource.URI == core.HiddenPromptContextURI {
				block = schema.ContentBlock{Kind: schema.ContentBlockKindText, Text: &schema.TextContent{Text: resource.Text}}
			}
		}
		blocks = append(blocks, block)
	}
	return blocks
}

// sessionUpdateIsRelayVisible keeps terminal output for live calls, while
// rejecting delayed history from `session/load` and private metadata that
// has no transcript projection.
func sessionUpdateIsRelayVisible(update *schema.SessionUpdate, liveToolCalls *Shared[map[string]struct{}], sessionID string) bool {
	panic("unported: mj-worker/src/acp.rs session_update_is_relay_visible")
}

func suppressCodexSandboxWarning(harness core.HarnessKind, policy core.ExecutionPolicy, update *schema.SessionUpdate) {
	panic("unported: mj-worker/src/acp.rs suppress_codex_sandbox_warning")
}

// removeSessionFailure keeps any title, goal, or other state carried
// alongside a suppressed notice.
func removeSessionFailure(update *schema.SessionUpdate) {
	panic("unported: mj-worker/src/acp.rs remove_session_failure")
}

// CommandRequest is a command the relay sends the ACP runtime.
type CommandRequest interface {
	isCommandRequest()
}

type CommandRequestClearContext struct {
	RequestID string
}

type CommandRequestPromptAttachments struct {
	RequestID string
	Prompt    []schema.ContentBlock
	Root      string
}

type CommandRequestPrompt struct {
	RequestID string
	Prompt    []schema.ContentBlock
}

type CommandRequestSetConfig struct {
	RequestID string
	Key       string
	Value     string
}

// CommandRequestGoalControl applies native goal control independently of the
// current prompt.
type CommandRequestGoalControl struct {
	RequestID string
	Action    core.GoalControlAction
}

// CommandRequestSetSessionMode selects an ACP session mode through
// `session/set_mode`.
type CommandRequestSetSessionMode struct {
	RequestID string
	ModeID    string
}

type CommandRequestRestoreExecutionMode struct {
	RequestID string
}

// CommandRequestResolveElicitation is the connection-only answer to an
// in-flight ACP elicitation. The content is deliberately never put in the
// durable relay command ledger. Resolved is Rust's oneshot sender: the
// sender makes it with room for one value, and the runtime sends it nil or
// the refusal exactly once, without blocking.
type CommandRequestResolveElicitation struct {
	ElicitationID string
	Response      core.ElicitationResponse
	Resolved      chan<- error
}

// CommandRequestStopBackgroundTask stops a background task. Resolved is a
// oneshot channel as for CommandRequestResolveElicitation.
type CommandRequestStopBackgroundTask struct {
	Target   core.BackgroundTaskStopTarget
	Resolved chan<- error
}

type CommandRequestCancelTurnFor struct {
	RequestID      string
	ActivePromptID string
}

type CommandRequestSteer struct {
	RequestID      string
	ActivePromptID string
	SteeringPrompt core.ClaimedSteeringPrompt
}

type CommandRequestCancel struct {
	RequestID      string
	SteeringPrompt *core.ClaimedSteeringPrompt
}

type CommandRequestClose struct {
	RequestID string
}

func (CommandRequestClearContext) isCommandRequest()         {}
func (CommandRequestPromptAttachments) isCommandRequest()    {}
func (CommandRequestPrompt) isCommandRequest()               {}
func (CommandRequestSetConfig) isCommandRequest()            {}
func (CommandRequestGoalControl) isCommandRequest()          {}
func (CommandRequestSetSessionMode) isCommandRequest()       {}
func (CommandRequestRestoreExecutionMode) isCommandRequest() {}
func (CommandRequestResolveElicitation) isCommandRequest()   {}
func (CommandRequestStopBackgroundTask) isCommandRequest()   {}
func (CommandRequestCancelTurnFor) isCommandRequest()        {}
func (CommandRequestSteer) isCommandRequest()                {}
func (CommandRequestCancel) isCommandRequest()               {}
func (CommandRequestClose) isCommandRequest()                {}

// asyncResult is what an async operation in flight delivers once: the
// Result<T> of a Rust boxed future.
type asyncResult[T any] struct {
	value T
	err   error
}

// ActivePrompt is the `session/prompt` request in flight; it delivers the
// agent's answer once.
type ActivePrompt = <-chan asyncResult[schema.PromptResponse]

// release is ActivePlanImplementation's Drop: it empties the slot, so a late
// permission answer cannot attach an implementation to a later prompt.
func (a ActivePlanImplementation) release() {
	sender, unlock := a.slot.Lock()
	defer unlock()
	*sender = nil
}

// Run runs the ACP runtime until it stops, without a shutdown signal.
func Run(spec LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent) error {
	return RunWithShutdown(context.Background(), spec, requests, events)
}

// RunWithShutdown stops protocol work cooperatively when ctx ends, while
// retaining bridge process cleanup.
func RunWithShutdown(ctx context.Context, spec LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent) error {
	result := runInner(ctx, spec, requests, events)
	// The final reports go out after shutdown too.
	reporting := context.WithoutCancel(ctx)
	if result != nil {
		fault := core.RuntimeEventSessionFault{Message: "ACP runtime failed: " + WorkerExitReason(result)}
		if err := emitRuntimeEvent(reporting, events, fault); err != nil {
			return fmt.Errorf("report ACP runtime failure: %v: %w", result, err)
		}
	}
	if err := emitRuntimeEvent(reporting, events, core.RuntimeEventStopped{}); err != nil {
		return err
	}
	return result
}

// OpenedSession is the native session a bridge opened.
type OpenedSession struct {
	nativeSessionID string
	startedAt       time.Time
	resumed         bool
	// nativeSessionUsed is set when a prompt may have reached the thread or
	// Claude ran a model cycle.
	nativeSessionUsed *atomic.Bool
}

// SessionRestart says how to reopen the native session on a fresh bridge.
type SessionRestart interface {
	isSessionRestart()
}

type SessionRestartResume struct {
	Value string
}

// SessionRestartReloadLogins reloads the idle native session on a fresh
// bridge that imports the changed staged logins (OpenCode).
type SessionRestartReloadLogins struct {
	Value string
}

type SessionRestartClear struct {
	Reset    ContextReset
	Previous string
}

func (SessionRestartResume) isSessionRestart()       {}
func (SessionRestartReloadLogins) isSessionRestart() {}
func (SessionRestartClear) isSessionRestart()        {}

// BridgeRestart is what runInner needs to start the next bridge.
type BridgeRestart struct {
	clearContext  *SessionRestartClear
	resumeSession *string
	// nativeSessionUsed is carried into the next bridge's spec: what the
	// dead bridge saw is the evidence for whether its thread may be
	// replaced.
	nativeSessionUsed bool
	sessionAge        time.Duration
	notice            BridgeRestartNotice
}

// BridgeRestartNotice is the notice a bridge restart shows.
type BridgeRestartNotice interface {
	isBridgeRestartNotice()
}

type BridgeRestartNoticePlanned struct {
	Value string
}

type BridgeRestartNoticeUnexpected struct{}

func (BridgeRestartNoticePlanned) isBridgeRestartNotice()    {}
func (BridgeRestartNoticeUnexpected) isBridgeRestartNotice() {}

// BridgeSessionOpen says how one bridge opens its native session.
// firstReplacedUnusedNativeSessionID is Rust's `&mut Option<String>`: the
// bridge records through it the first unused thread it replaced.
type BridgeSessionOpen struct {
	replacingPreviousBridge            bool
	replaceUnusedCodexThread           bool
	firstReplacedUnusedNativeSessionID **string
}

func runInner(ctx context.Context, spec LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent) error {
	panic("unported: mj-worker/src/acp.rs run_inner")
}

// runBridge runs one ACP bridge process. A restart means reopen the native
// session on a fresh bridge: a cancel that never acked, or a dead ACP child.
// ctx is the runtime's shutdown signal.
func runBridge(ctx context.Context, spec *LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent, opened *Shared[*OpenedSession], sessionOpen BridgeSessionOpen) (*BridgeRestart, error) {
	panic("unported: mj-worker/src/acp.rs run_bridge")
}

const acpStderrTailBytes = 16 * 1024

// adapterChatterPrefix is chatter the Claude bridge logs for SDK events it
// does not model, for example `Unexpected case: {"type":"vcs_state_changed"}`.
// It arrives often enough to fill the whole stderr tail and bury the real
// failure in worker exit records.
const adapterChatterPrefix = "Unexpected case: "

// kimiNullResponseChatter: Kimi 0.37.x logs this for response-shaped
// startup frames with a null id. It is adapter routing noise and commonly
// precedes a useful ACP error.
const kimiNullResponseChatter = "Got response to unknown request null"

func unsupportedClientRequestReport(method string) string {
	return fmt.Sprintf("The agent sent the client request %s, which Hel does not implement. "+
		"Hel answered with a method-not-found error rather than leaving the agent waiting.", method)
}

// actionableStderrTail is the part of a bridge stderr tail worth attaching to
// a failing result. Returns false when only adapter chatter was captured, so
// a failure keeps its own error text instead of gaining misleading context.
func actionableStderrTail(tail string) (string, bool) {
	panic("unported: mj-worker/src/acp.rs actionable_stderr_tail")
}

// errRelayEventCoordinatorStopped is the error of an event the relay's
// event coordinator is no longer there to receive.
var errRelayEventCoordinatorStopped = errors.New("relay event coordinator stopped")

// emitRuntimeEvent sends event to the relay's event coordinator, waiting for
// room as Rust's bounded channel does, and fails once ctx ends.
func emitRuntimeEvent(ctx context.Context, events chan<- core.RuntimeEvent, event core.RuntimeEvent) error {
	select {
	case events <- event:
		return nil
	default:
	}
	select {
	case events <- event:
		return nil
	case <-ctx.Done():
		return errRelayEventCoordinatorStopped
	}
}

// emitCloseOutcome emits the runtime events for a finished `session/close`
// request; outcome is its error, nil when it succeeded.
//
// A harness that answers "method not found" does not implement
// `session/close`, so it has no session state to release and the worker
// tears its runtime down after this either way. Treat that answer as
// applied; rejecting it leaves the session stuck in "closing" forever.
func emitCloseOutcome(ctx context.Context, events chan<- core.RuntimeEvent, requestID string, outcome error) error {
	if outcome == nil {
		return emitRuntimeEvent(ctx, events, core.RuntimeEventCloseApplied{RequestID: requestID})
	}
	var rpc *acp.RPCError
	if errors.As(outcome, &rpc) && rpc.Code == rpcCodeMethodNotFound {
		warning := core.RuntimeEventWarning{Message: "the harness has no session/close method; closing its runtime instead"}
		if err := emitRuntimeEvent(ctx, events, warning); err != nil {
			return err
		}
		return emitRuntimeEvent(ctx, events, core.RuntimeEventCloseApplied{RequestID: requestID})
	}
	return emitRuntimeEvent(ctx, events, core.RuntimeEventCommandRejected{
		Reason:    core.OutcomeReasonCommandFailed,
		RequestID: requestID,
		Message:   "close ACP session: " + acpErrorDisplay(outcome),
	})
}

// unknownTerminalError answers a `terminal/*` request naming a terminal this
// connection does not have, most often one the agent already released.
func unknownTerminalError(terminalID string) *acp.RPCError {
	return rpcErrorWithData(rpcCodeInvalidParams, "Invalid params", "unknown terminal "+terminalID)
}

func terminalExitStatus(exit *TerminalExit) schema.TerminalExitStatus {
	return schema.TerminalExitStatus{ExitCode: exit.ExitCode, Signal: exit.Signal}
}

func relayEventChannelError() *acp.RPCError {
	return rpcErrorWithData(rpcCodeInternalError, "Internal error", "relay event coordinator stopped")
}

// mergeDriveError adds additional to a drive's result: Rust's
// `merge_drive_error(&mut Result<Option<SessionRestart>>, ..)` over the Go
// pair (restart, result).
func mergeDriveError(restart *SessionRestart, result *error, additional error) {
	if *result == nil {
		*restart = nil
		*result = additional
		return
	}
	*result = fmt.Errorf("additional ACP runtime error: %v: %w", additional, *result)
}

// bridgeStdoutRule says what a bridge's stdout may carry, for errors that
// follow a broken transport.
const bridgeStdoutRule = "bridge stdout must contain only JSON-RPC frames once the " +
	"first frame arrives; non-JSON lines before it are logged and skipped"

// skipStdoutPreamble passes a bridge's stdout through to the ACP transport,
// dropping the non-JSON lines a launcher or login shell prints before the
// bridge's first JSON-RPC frame. Each dropped line is logged. The ACP
// transport answers every unparsable line with a parse error, and a Kimi
// launcher's installer output ended a session that way (#1136). From the
// first frame on, bytes pass through unchanged, so the transport stays
// strict. A bridge that never sends a frame still fails at the initialize
// timeout.
func skipStdoutPreamble(stdout io.Reader) io.ReadCloser {
	panic("unported: mj-worker/src/acp.rs skip_stdout_preamble")
}

func readStderrTail(stderr io.Reader) (string, error) {
	panic("unported: mj-worker/src/acp.rs read_stderr_tail")
}

const acpBridgeLostWarning = "ACP bridge exited; reloading the native session"

const acpBridgeRestartWarning = "ACP bridge restarting; reloading the native session"

const opencodeLoginsReloadMessage = "The profile's OpenCode logins changed; reloading the session to use them"

// ResolveKimiSessionDir is kimi_tasks::resolve_session_dir, which acp.rs
// re-exports under this name.
func ResolveKimiSessionDir(kimiHome string, sessionID string) (string, error) {
	return ResolveSessionDir(kimiHome, sessionID)
}

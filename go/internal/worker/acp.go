// Port of mj-worker/src/acp.rs.

// ACP runtime and normalized session controls used by a Hel session worker.
//
// The worker owns exactly one harness process and one foreground session. It
// deliberately does not know about orchestration, turn reviews, or subagents;
// core's ACP surface projects protocol capabilities for the chat control
// surface.

package worker

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
	"unicode"
	"unicode/utf8"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
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
	// rpcCodeRequestCancelled is the SDK's request_cancelled code.
	rpcCodeRequestCancelled = -32800
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
	text := err.Message
	if text == "" {
		text = strconv.Itoa(err.Code)
	}
	// serde reads `"data": null` as no data.
	if data := bytes.TrimSpace(err.Data); len(data) > 0 && string(data) != "null" {
		value, parseErr := jsonvalue.Parse(data)
		if parseErr != nil {
			text += ": " + string(data)
		} else {
			text += ": " + acpPrettyJSON(value)
		}
	}
	return text
}

// acpPrettyJSON is serde_json::to_string_pretty of a jsonvalue value: two
// spaces of indentation, `": "` between a key and its value, and `[]` and
// `{}` for empty arrays and objects.
func acpPrettyJSON(value any) string {
	compact, err := jsonvalue.Marshal(value)
	if err != nil {
		return fmt.Sprint(value)
	}
	var pretty bytes.Buffer
	if err := json.Indent(&pretty, compact, "", "  "); err != nil {
		return string(compact)
	}
	return pretty.String()
}

// acpInternalError is agent_client_protocol::Error::internal_error()
// carrying data as its string data.
func acpInternalError(data string) *acp.RPCError {
	return rpcErrorWithData(rpcCodeInternalError, "Internal error", data)
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

// acpCallError is an error a Call returned, as Rust shows it: an ACP RPC
// error keeps the SDK's Display (acpDisplayError); a connection that ended
// becomes the internal error the Rust SDK reports for a request the
// connection can no longer answer.
func acpCallError(err error) error {
	if err == nil {
		return nil
	}
	var rpc *acp.RPCError
	if errors.As(err, &rpc) {
		return acpDisplayError{rpc}
	}
	return acpDisplayError{acpInternalError(err.Error())}
}

// acpRPCError is the ACP error a failed Call carries, for code that reads
// its code and data: the RPC error itself, or the internal error a
// connection failure becomes (acpCallError).
func acpRPCError(err error) *acp.RPCError {
	var rpc *acp.RPCError
	if errors.As(err, &rpc) {
		return rpc
	}
	return acpInternalError(err.Error())
}

// acpCall is connection.send_request(params).block_task(): it waits for the
// answer, decoding it into result, and returns the failure as Rust shows it
// (acpCallError). The wait is never cancelled from here: a request the
// session abandons ends with its connection, as Rust's connection disarms
// cancellation traffic while it drops its consumers.
func acpCall(ctx context.Context, connection *acp.Connection, method string, params, result any) error {
	return acpCallError(connection.Call(context.WithoutCancel(ctx), method, params, result))
}

// acpStartCall sends a request and returns the channel its answer arrives
// on, once: Rust's pinned `send_request(..).block_task()` future. drop is
// dropping that future: a request still waiting for its answer is
// cancelled with `$/cancel_request`, as Rust's connection does while it
// runs; dropping one already answered does nothing.
func acpStartCall[T any](ctx context.Context, connection *acp.Connection, method string, params any) (answer <-chan asyncResult[T], drop context.CancelFunc) {
	answered := make(chan asyncResult[T], 1)
	callCtx, cancel := context.WithCancel(context.WithoutCancel(ctx))
	go func() {
		defer cancel()
		var result T
		err := acpCallError(connection.Call(callCtx, method, params, &result))
		answered <- asyncResult[T]{value: result, err: err}
	}()
	return answered, cancel
}

// acpErrorChain is anyhow's chain(): the message of each layer of err on
// its own, outermost first. A layer made with fmt.Errorf("context: %w")
// shows its context alone; an ACP error shows the SDK's Display and ends
// the chain, as the Rust error it stands for does.
func acpErrorChain(err error) []string {
	var messages []string
	for err != nil {
		switch layer := err.(type) {
		case acpDisplayError:
			return append(messages, acpErrorText(layer.err))
		case *acp.RPCError:
			return append(messages, acpErrorText(layer))
		}
		text := err.Error()
		next := errors.Unwrap(err)
		if next == nil {
			return append(messages, text)
		}
		own, ok := strings.CutSuffix(text, ": "+next.Error())
		if !ok {
			return append(messages, text)
		}
		messages = append(messages, own)
		err = next
	}
	return messages
}

// acpIOError is err as Rust's io::Error displays it: an OS error shows the
// system's description and its number (`No such file or directory (os
// error 2)`) without Go's operation and path prefix.
func acpIOError(err error) string {
	var errno syscall.Errno
	if !errors.As(err, &errno) {
		if errors.Is(err, exec.ErrNotFound) {
			return "No such file or directory (os error 2)"
		}
		return err.Error()
	}
	description := errno.Error()
	if description != "" {
		first, size := utf8.DecodeRuneInString(description)
		description = string(unicode.ToUpper(first)) + description[size:]
	}
	return fmt.Sprintf("%s (os error %d)", description, int(errno))
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
	var tails, causes []string
	for _, text := range acpErrorChain(err) {
		if strings.HasPrefix(text, bridgeStderrContext) {
			tails = append(tails, text)
		} else {
			causes = append(causes, text)
		}
	}
	var cause string
	var failure core.WorkerHarnessStartupFailure
	var failurePointer *core.WorkerHarnessStartupFailure
	switch {
	case errors.As(err, &failure):
		cause = failure.Message
	case errors.As(err, &failurePointer) && failurePointer != nil:
		cause = failurePointer.Message
	default:
		lines := make([]string, len(causes))
		for i, text := range causes {
			lines[i] = singleSpaced(text)
		}
		cause = strings.Join(lines, ": ")
	}
	for _, tail := range tails {
		cause = cause + "\n" + tail
	}
	return cause
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

// toolCallUpdateFieldsEmpty says whether a tool call update carries none of
// ToolCallUpdateFields (Rust compares them with the default).
func toolCallUpdateFieldsEmpty(update *schema.ToolCallUpdate) bool {
	return update.Kind == nil && update.Status == nil && update.Title == nil && update.Name == nil &&
		update.Content == nil && update.Locations == nil && acpRawValueAbsent(update.RawInput) &&
		acpRawValueAbsent(update.RawOutput)
}

// acpRawValueAbsent says whether an `Option<serde_json::Value>` member is
// None: absent, or null, which serde reads as None.
func acpRawValueAbsent(raw json.RawMessage) bool {
	trimmed := bytes.TrimSpace(raw)
	return len(trimmed) == 0 || string(trimmed) == "null"
}

// sessionUpdateIsRelayVisible keeps terminal output for live calls, while
// rejecting delayed history from `session/load` and private metadata that
// has no transcript projection.
func sessionUpdateIsRelayVisible(update *schema.SessionUpdate, liveToolCalls *Shared[map[string]struct{}], sessionID string) bool {
	switch {
	// The accepted command is the authoritative user message. Agent echoes
	// have no projection and must not put image bytes back into the journal.
	case update.UserMessageChunk != nil:
		return false
	case update.ToolCall != nil:
		calls, unlock := liveToolCalls.Lock()
		if *calls == nil {
			*calls = map[string]struct{}{}
		}
		(*calls)[string(update.ToolCall.ToolCallID)] = struct{}{}
		unlock()
		return true
	case update.ToolCallUpdate != nil:
		call := update.ToolCallUpdate
		if toolCallUpdateFieldsEmpty(call) {
			terminal := false
			for _, key := range []string{"terminal_output", "terminal_output_delta", "terminal_exit"} {
				if _, ok := call.Meta.Get(key); ok {
					terminal = true
				}
			}
			if !terminal {
				return false
			}
		}
		calls, unlock := liveToolCalls.Lock()
		_, createdLive := (*calls)[string(call.ToolCallID)]
		unlock()
		if !createdLive {
			slog.Warn("ignored delayed ACP update for a tool call not created on this live connection",
				"session_id", sessionID, "tool_call_id", string(call.ToolCallID))
		}
		return createdLive
	}
	return true
}

// codexBubblewrapWarning is the warning Codex raises when bubblewrap is
// missing, which only matters to a sandboxed session.
const codexBubblewrapWarning = "Codex could not find bubblewrap on PATH. " +
	"Install bubblewrap with your OS package manager. See the sandbox prerequisites: " +
	"https://developers.openai.com/codex/concepts/sandboxing#prerequisites. " +
	"Codex will use the bundled bubblewrap in the meantime."

func suppressCodexSandboxWarning(harness core.HarnessKind, policy core.ExecutionPolicy, update *schema.SessionUpdate) {
	if harness != core.HarnessKindCodex || !policy.IsUnconstrained() {
		return
	}
	info := update.SessionInfoUpdate
	if info == nil {
		return
	}
	failure := core.SessionFailureFromMeta(info.Meta)
	if failure == nil {
		return
	}
	if failure.Severity != "warning" || failure.Details != nil || failure.Title != codexBubblewrapWarning {
		return
	}
	// Codex checks sandbox prerequisites before opening a full-access thread.
	removeSessionFailure(update)
	slog.Debug("suppressed Codex bubblewrap prerequisite warning in YOLO mode")
}

// removeSessionFailure keeps any title, goal, or other state carried
// alongside a suppressed notice.
func removeSessionFailure(update *schema.SessionUpdate) {
	info := update.SessionInfoUpdate
	if info == nil {
		return
	}
	jetbrainsValue, ok := info.Meta.Get("jetbrains")
	if !ok {
		return
	}
	jetbrains, ok := jetbrainsValue.(jsonvalue.Object)
	if !ok {
		return
	}
	airValue, ok := jetbrains.Get("air")
	if !ok {
		return
	}
	air, ok := airValue.(jsonvalue.Object)
	if !ok {
		return
	}
	// The objects are shared with the decoded update's other readers, so the
	// change is made on copies and put back.
	air = air.Clone()
	if !air.Delete("sessionFailure") {
		return
	}
	jetbrains = jetbrains.Clone()
	jetbrains.Set("air", air)
	meta := info.Meta.Clone()
	meta.Set("jetbrains", jetbrains)
	changed := *info
	changed.Meta = meta
	update.SessionInfoUpdate = &changed
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

// CommandRequestReloadLaunchConfig restarts only the harness bridge process
// under a new launch configuration without stopping the worker daemon or its
// non-harness processes. A nil Command or Args keeps the current one. Leases
// stay held while the replacement bridge runs. Resolved is a oneshot channel
// as for CommandRequestResolveElicitation: nil once the replacement bridge
// has opened its session, or the refusal.
type CommandRequestReloadLaunchConfig struct {
	LaunchConfig          *core.WorkerLaunchConfig
	Environment           map[string]string
	Command               *string
	Args                  *[]string
	AdditionalDirectories []string
	SupervisorSpec        *ReloadSupervisorSpec
	Leases                ReloadLeases
	Resolved              chan<- error
}

// ReloadSupervisorSpec is the supervisor spec a reload writes before its
// bridge starts, and where.
type ReloadSupervisorSpec struct {
	Path string
	Spec AcpSupervisorSpec
}

// ReloadLeases is what a reload keeps alive while its bridge runs (Rust a
// boxed value whose Drop releases it). Release is that Drop; nil holds
// nothing.
type ReloadLeases interface {
	Release()
}

// releaseReloadLeases drops leases: Rust drops them with the reload that
// carried them.
func releaseReloadLeases(leases ReloadLeases) {
	if leases != nil {
		leases.Release()
	}
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
func (CommandRequestReloadLaunchConfig) isCommandRequest()   {}
func (CommandRequestCancelTurnFor) isCommandRequest()        {}
func (CommandRequestSteer) isCommandRequest()                {}
func (CommandRequestCancel) isCommandRequest()               {}
func (CommandRequestClose) isCommandRequest()                {}

// sendResolution answers a oneshot resolution channel without blocking, as
// Rust's oneshot send does; false when nobody can take the answer.
func sendResolution(resolved chan<- error, outcome error) bool {
	if resolved == nil {
		return false
	}
	select {
	case resolved <- outcome:
		return true
	default:
		return false
	}
}

// asyncResult is what an async operation in flight delivers once: the
// Result<T> of a Rust boxed future.
type asyncResult[T any] struct {
	value T
	err   error
}

// ActivePrompt is the `session/prompt` request in flight; it delivers the
// agent's answer once. The answer is read with the unstable schema, whose
// PromptResponse carries the turn's token usage (Rust's
// `unstable_end_turn_token_usage`).
type ActivePrompt = <-chan asyncResult[promptAnswer]

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
	return runWithGuardian(ctx, spec, requests, events, nil)
}

// runWithGuardian is RunWithShutdown whose guardian reviews through the
// worker's daemon guardian endpoint; a nil endpoint reviews over ACP only.
func runWithGuardian(ctx context.Context, spec LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent, guardianEndpoint *GuardianEndpoint) error {
	result := runInner(ctx, spec, requests, events, guardianEndpoint)
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

// SessionRestartReloadLaunchConfig restarts only the harness bridge under a
// new launch configuration. ResumeSession is the native session the next
// bridge reopens, nil for a new one.
type SessionRestartReloadLaunchConfig struct {
	LaunchConfig          *core.WorkerLaunchConfig
	Environment           map[string]string
	Command               *string
	Args                  *[]string
	AdditionalDirectories []string
	SupervisorSpec        *ReloadSupervisorSpec
	Leases                ReloadLeases
	ResumeSession         *string
	Resolved              chan<- error
}

type SessionRestartClear struct {
	Reset    ContextReset
	Previous string
}

func (SessionRestartResume) isSessionRestart()             {}
func (SessionRestartReloadLogins) isSessionRestart()       {}
func (SessionRestartReloadLaunchConfig) isSessionRestart() {}
func (SessionRestartClear) isSessionRestart()              {}

// releaseSessionRestart drops a restart nobody acts on: Rust drops the
// leases of a launch configuration reload with it.
func releaseSessionRestart(restart SessionRestart) {
	if reload, ok := restart.(SessionRestartReloadLaunchConfig); ok {
		releaseReloadLeases(reload.Leases)
	}
}

// launchConfigReload is the launch configuration a planned restart applies
// before the next bridge starts.
type launchConfigReload struct {
	launchConfig          *core.WorkerLaunchConfig
	environment           map[string]string
	command               *string
	args                  *[]string
	additionalDirectories []string
	supervisorSpec        *ReloadSupervisorSpec
	leases                ReloadLeases
	resolved              chan<- error
}

// BridgeRestart is what runInner needs to start the next bridge.
type BridgeRestart struct {
	clearContext       *SessionRestartClear
	resumeSession      *string
	reloadLaunchConfig *launchConfigReload
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
// pendingReloadAck is likewise Rust's `&mut Option<oneshot::Sender<..>>`: the
// reload this bridge's open acknowledges, taken (set to nil) once answered.
type BridgeSessionOpen struct {
	replacingPreviousBridge            bool
	replaceUnusedCodexThread           bool
	firstReplacedUnusedNativeSessionID **string
	pendingReloadAck                   *chan<- error
}

// takePendingReloadAck is Rust's `pending_reload_ack.take()`: the waiting
// reload's resolution, nil when none waits.
func takePendingReloadAck(pending *chan<- error) chan<- error {
	if pending == nil {
		return nil
	}
	resolved := *pending
	*pending = nil
	return resolved
}

// clearRollback is what a failed clear restores: the conversation it
// replaced, its history evidence, and its goal.
type clearRollback struct {
	previous string
	history  NativeSessionHistory
	goal     *Shared[core.GoalRecoveryContext]
}

func runInner(ctx context.Context, spec LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent, guardianEndpoint *GuardianEndpoint) error {
	environment, err := harnessinstall.WithOverrides(ctx, spec.Environment)
	if err != nil {
		return err
	}
	spec.Environment = environment
	// Reports wait for the relay as Rust's sends do; shutdown is observed at
	// the top of the loop, not by abandoning a report.
	reporting := context.WithoutCancel(ctx)
	rapidDeaths := uint32(0)
	replacingPreviousBridge := false
	var restartNotice BridgeRestartNotice
	// A new worker starts from the relay journal's latest native session ID.
	// Within this worker, restore readiness can first see any replacement.
	var firstReplacedUnusedNativeSessionID *string
	var pendingReloadAck chan<- error
	var activeReloadLeases ReloadLeases
	defer func() { releaseReloadLeases(activeReloadLeases) }()
	var rollback *clearRollback
	for {
		if ctx.Err() != nil {
			if resolved := takePendingReloadAck(&pendingReloadAck); resolved != nil {
				sendResolution(resolved, errors.New("ACP runtime shut down before replacement bridge opened"))
			}
			return nil
		}
		replaceUnusedCodexThread := shouldReplaceUnusedCodexThread(&spec)
		if notice := restartNotice; notice != nil {
			restartNotice = nil
			var message string
			switch notice := notice.(type) {
			case BridgeRestartNoticePlanned:
				message = notice.Value
			case BridgeRestartNoticeUnexpected:
				if replaceUnusedCodexThread {
					message = "The agent stopped before its first prompt; restarting with a new empty thread."
				} else {
					message = acpBridgeLostWarning
				}
			}
			if err := emitRuntimeEvent(reporting, events, core.RuntimeEventHarnessRestarting{Message: message}); err != nil {
				return err
			}
		}
		opened := NewShared[*OpenedSession](nil)
		restart, result := runBridge(ctx, &spec, requests, events, opened, BridgeSessionOpen{
			replacingPreviousBridge:            replacingPreviousBridge,
			replaceUnusedCodexThread:           replaceUnusedCodexThread,
			firstReplacedUnusedNativeSessionID: &firstReplacedUnusedNativeSessionID,
			pendingReloadAck:                   &pendingReloadAck,
		}, guardianEndpoint)
		session, unlock := opened.Lock()
		openedSession := *session
		unlock()
		if openedSession != nil {
			spec.ContextRestore = nil
			if !openedSession.resumed {
				// session/new establishes the proof for this new identity,
				// including when it just replaced another unused thread.
				spec.NativeSessionHistory = NativeSessionHistoryCreatedHereUnused
			} else if spec.NativeSessionHistory == NativeSessionHistoryCreatedHereUnused {
				spec.NativeSessionHistory = NativeSessionHistoryLoadBeforeReplace
			}
			if spec.ClearContextRequest != nil {
				spec.ClearContextRequest = nil
				rollback = nil
			}
		}
		if result != nil && spec.ClearContextRequest != nil {
			reset := spec.ClearContextRequest
			spec.ClearContextRequest = nil
			spec.ContextRestore = reset
			if err := emitRuntimeEvent(reporting, events, core.RuntimeEventCommandRejected{
				RequestID: reset.RequestID,
				Message:   "Could not clear context; restoring the previous conversation: " + result.Error(),
				Reason:    core.OutcomeReasonCommandFailed,
			}); err != nil {
				return err
			}
			if rollback == nil {
				panic("clear rollback identity")
			}
			previous := rollback.previous
			spec.ResumeSession = &previous
			spec.NativeSessionHistory = rollback.history
			spec.GoalRecovery = rollback.goal
			rollback = nil
			replacingPreviousBridge = true
			continue
		}
		if result != nil {
			if resolved := takePendingReloadAck(&pendingReloadAck); resolved != nil {
				sendResolution(resolved, errors.New(result.Error()))
			}
			return result
		}
		if restart == nil {
			if resolved := takePendingReloadAck(&pendingReloadAck); resolved != nil {
				sendResolution(resolved, errors.New("ACP runtime stopped before replacement bridge opened"))
			}
			return nil
		}
		if restart.nativeSessionUsed {
			spec.NativeSessionHistory = NativeSessionHistoryMayHaveHistory
		}
		if clear := restart.clearContext; clear != nil {
			// runBridge has reaped the old process. Retire its relay-owned
			// work before either the replacement or rollback bridge opens.
			if err := emitRuntimeEvent(reporting, events, core.RuntimeEventContextClearing{RequestID: clear.Reset.RequestID}); err != nil {
				return err
			}
			rollback = &clearRollback{previous: clear.Previous, history: spec.NativeSessionHistory, goal: spec.GoalRecovery}
			spec.GoalRecovery = NewShared(core.GoalRecoveryContext{})
			reset := clear.Reset
			spec.ClearContextRequest = &reset
			spec.ResumeSession = nil
			spec.NativeSessionHistory = NativeSessionHistoryLoadBeforeReplace
			replacingPreviousBridge = true
			continue
		}
		if _, unexpected := restart.notice.(BridgeRestartNoticeUnexpected); unexpected {
			if restart.sessionAge < rapidBridgeWindow {
				rapidDeaths++
				if rapidDeaths >= rapidBridgeRestartLimit {
					return errors.New("ACP bridge exited repeatedly during startup; giving up")
				}
			} else {
				rapidDeaths = 0
			}
		}
		restartNotice = restart.notice
		if reload := restart.reloadLaunchConfig; reload != nil {
			var resolvedEnvironment map[string]string
			var environmentErr error
			if reload.supervisorSpec != nil {
				// PrepareHarnessLaunch already merged the login environment
				// and applied `exclude_from_harness_environment`; rerunning
				// WithOverrides would reintroduce excluded shell variables.
				resolvedEnvironment = reload.environment
			} else {
				resolvedEnvironment, environmentErr = harnessinstall.WithOverrides(ctx, reload.environment)
			}
			if supervisor := reload.supervisorSpec; supervisor != nil {
				if err := supervisor.Spec.writeSpec(supervisor.Path); err != nil {
					sendResolution(reload.resolved, errors.New("write updated ACP supervisor spec: "+err.Error()))
					releaseReloadLeases(reload.leases)
					return err
				}
			}
			if environmentErr != nil {
				sendResolution(reload.resolved, errors.New(environmentErr.Error()))
				releaseReloadLeases(reload.leases)
				return environmentErr
			}
			launchConfig := reload.launchConfig
			spec.applyReloadAcceptedConfig(launchConfig)
			spec.Environment = resolvedEnvironment
			if reload.command != nil {
				spec.Command = *reload.command
			}
			if reload.args != nil {
				spec.Args = *reload.args
			}
			spec.Cwd = launchConfig.Cwd
			spec.AdditionalDirectories = reload.additionalDirectories
			spec.Harness = launchConfig.Harness
			spec.InitialModel = launchConfig.InitialModel
			spec.ExecutionPolicy = launchConfig.ExecutionPolicy
			// Rust: WorkerLaunchConfig::approval_choices.
			spec.ApprovalChoices = core.ApprovalChoices{
				ClaudePermissionMode: launchConfig.ClaudePermissionMode,
				MjolnirGuardian:      launchConfig.MjolnirGuardian,
			}
			spec.InstructionRole = core.InstructionRoleFromLaunch(launchConfig)
			spec.ProjectMemory = launchConfig.ProjectMemory
			releaseReloadLeases(activeReloadLeases)
			activeReloadLeases = reload.leases
			pendingReloadAck = reload.resolved
		}
		goal, unlockGoal := spec.GoalRecovery.Lock()
		goal.State.Restart()
		unlockGoal()
		spec.ResumeSession = restart.resumeSession
		replacingPreviousBridge = true
	}
}

// bridgeChild is the bridge process and the pipes this side keeps of it.
// Its wait runs once, in the background, and is read from exited; Rust's
// child.wait() reaps the same way without closing the pipes.
type bridgeChild struct {
	command *exec.Cmd
	exited  chan struct{}
	state   *os.ProcessState
	waitErr error
}

func (c *bridgeChild) wait(timeout time.Duration) bool {
	if timeout < 0 {
		<-c.exited
		return true
	}
	timer := time.NewTimer(timeout)
	defer timer.Stop()
	select {
	case <-c.exited:
		return true
	case <-timer.C:
		return false
	}
}

// bridgeExitStatus is a process exit as Rust's ExitStatus Display shows
// it: `exit status: N`, or `signal: N (SIGNAME)` for a signal.
func bridgeExitStatus(state *os.ProcessState) string {
	if state == nil {
		return "unknown status"
	}
	if status, ok := state.Sys().(syscall.WaitStatus); ok && status.Signaled() {
		signal := int(status.Signal())
		text := fmt.Sprintf("signal: %d", signal)
		if name := bridgeSignalName(status.Signal()); name != "" {
			text += " (" + name + ")"
		}
		if status.CoreDump() {
			text += " (core dumped)"
		}
		return text
	}
	return fmt.Sprintf("exit status: %d", state.ExitCode())
}

// bridgeExecutable is the program a bridge runs. Rust's Command searches a
// bare name on the PATH the child is given, not the worker's own.
func bridgeExecutable(command string, environment map[string]string) (string, error) {
	if strings.ContainsRune(command, filepath.Separator) || strings.ContainsRune(command, '/') {
		return command, nil
	}
	path, ok := environment["PATH"]
	if !ok {
		return exec.LookPath(command)
	}
	for _, directory := range filepath.SplitList(path) {
		if directory == "" {
			directory = "."
		}
		candidate := filepath.Join(directory, command)
		if info, err := os.Stat(candidate); err == nil && !info.IsDir() && info.Mode()&0o111 != 0 {
			return candidate, nil
		}
	}
	return "", &exec.Error{Name: command, Err: exec.ErrNotFound}
}

// startBridgeProcess starts the bridge process with exactly the spec's
// environment, its working directory, and piped standard streams.
func startBridgeProcess(spec *LaunchSpec) (*bridgeChild, io.WriteCloser, io.ReadCloser, io.ReadCloser, error) {
	launchError := func(err error) error {
		if errors.Is(err, fs.ErrNotExist) || errors.Is(err, exec.ErrNotFound) {
			if info, statErr := os.Stat(spec.Cwd); statErr != nil || !info.IsDir() {
				return fmt.Errorf("ACP bridge working directory %s does not exist or is not a directory: %s",
					spec.Cwd, acpIOError(err))
			}
			return fmt.Errorf("ACP bridge executable %s was not found while starting in working directory %s: %s",
				spec.Command, spec.Cwd, acpIOError(err))
		}
		return fmt.Errorf("launch ACP bridge %s in working directory %s: %s", spec.Command, spec.Cwd, acpIOError(err))
	}
	program, err := bridgeExecutable(spec.Command, spec.Environment)
	if err != nil {
		return nil, nil, nil, nil, launchError(err)
	}
	stdinRead, stdinWrite, err := os.Pipe()
	if err != nil {
		return nil, nil, nil, nil, launchError(err)
	}
	stdoutRead, stdoutWrite, err := os.Pipe()
	if err != nil {
		stdinRead.Close()
		stdinWrite.Close()
		return nil, nil, nil, nil, launchError(err)
	}
	stderrRead, stderrWrite, err := os.Pipe()
	if err != nil {
		for _, file := range []*os.File{stdinRead, stdinWrite, stdoutRead, stdoutWrite} {
			file.Close()
		}
		return nil, nil, nil, nil, launchError(err)
	}
	command := exec.Command(program, spec.Args...)
	command.Args[0] = spec.Command
	command.Env = make([]string, 0, len(spec.Environment))
	for name, value := range spec.Environment {
		command.Env = append(command.Env, name+"="+value)
	}
	command.Dir = spec.Cwd
	command.Stdin = stdinRead
	command.Stdout = stdoutWrite
	command.Stderr = stderrWrite
	startErr := core.SpawnChild(command)
	// The child holds its own ends now; this side keeps only its ends.
	stdinRead.Close()
	stdoutWrite.Close()
	stderrWrite.Close()
	if startErr != nil {
		stdinWrite.Close()
		stdoutRead.Close()
		stderrRead.Close()
		return nil, nil, nil, nil, launchError(startErr)
	}
	child := &bridgeChild{command: command, exited: make(chan struct{})}
	go func() {
		defer close(child.exited)
		child.waitErr = command.Wait()
		child.state = command.ProcessState
	}()
	return child, stdinWrite, stdoutRead, stderrRead, nil
}

// runBridge runs one ACP bridge process. A restart means reopen the native
// session on a fresh bridge: a cancel that never acked, or a dead ACP child.
// ctx is the runtime's shutdown signal.
func runBridge(ctx context.Context, spec *LaunchSpec, requests <-chan CommandRequest, events chan<- core.RuntimeEvent, opened *Shared[*OpenedSession], sessionOpen BridgeSessionOpen, guardianEndpoint *GuardianEndpoint) (*BridgeRestart, error) {
	// Before the process starts, not after: a selector the harness reads only
	// at startup cannot be corrected once it has opened the session.
	if runtime.GOOS != "windows" && spec.BridgeSpecPath != nil {
		if err := repinBridgeSelectors(*spec.BridgeSpecPath, spec.Harness, spec.StartupModel()); err != nil {
			return nil, fmt.Errorf("pin this session's model into the ACP bridge launch: %w", err)
		}
	}
	launch := spec.Clone()
	if launch.Harness.KeepsLoginsInDatabase() {
		imported, err := importedLoginsPrepare(launch.Environment)
		if err != nil {
			return nil, err
		}
		launch.ImportedLogins = &imported
	}
	// Before the process starts, like the pinned selectors: agy reads its
	// grants at startup.
	if err := stageAntigravityGrants(&launch); err != nil {
		return nil, err
	}
	child, stdin, stdout, stderr, err := startBridgeProcess(&launch)
	if err != nil {
		return nil, err
	}
	// kill_on_drop: whatever happens below, the child does not outlive this
	// bridge.
	defer func() {
		select {
		case <-child.exited:
		default:
			_ = child.command.Process.Kill()
		}
	}()
	type stderrTail struct {
		tail string
		err  error
	}
	stderrDone := make(chan stderrTail, 1)
	go func() {
		defer func() {
			if recovered := recover(); recovered != nil {
				stderrDone <- stderrTail{err: fmt.Errorf("ACP stderr collector task failed: %v", recovered)}
			}
		}()
		tail, err := readStderrTail(stderr)
		stderrDone <- stderrTail{tail: tail, err: err}
	}()
	transport := NewByteStreams(stdin, skipStdoutPreamble(stdout))

	terminals, err := NewTerminalRegistry()
	if err != nil {
		stdin.Close()
		return nil, err
	}
	// Like the terminals, the guardian's reviewer outlives the protocol
	// driver: it is stopped below, after this bridge's own teardown, on every
	// exit path, so the next bridge never starts beside a live reviewer.
	guardian := guardianForLaunch(&launch, guardianEndpoint)
	type driven struct {
		restart SessionRestart
		err     error
	}
	driveCtx, stopDrive := context.WithCancel(ctx)
	defer stopDrive()
	driveDone := make(chan driven, 1)
	go func() {
		restart, err := driveWithTerminals(driveCtx, transport, launch.Clone(), requests, events, opened, sessionOpen,
			BridgeOwned{terminals: terminals, guardian: guardian})
		driveDone <- driven{restart: restart, err: err}
	}()
	var restart SessionRestart
	var result error
	childReaped := false
	select {
	case <-ctx.Done():
	case outcome := <-driveDone:
		restart, result = outcome.restart, outcome.err
		driveDone = nil
	case <-child.exited:
	}
	// Biased as Rust's select: shutdown, then the driver's result, then the
	// child's exit.
	switch {
	case ctx.Err() != nil:
		releaseSessionRestart(restart)
		restart, result = nil, nil
	case driveDone == nil:
	default:
		select {
		case outcome := <-driveDone:
			restart, result = outcome.restart, outcome.err
			driveDone = nil
		default:
			childReaped = true
			if child.waitErr != nil && child.state == nil {
				result = fmt.Errorf("wait for ACP bridge: %w", child.waitErr)
			} else {
				result = fmt.Errorf("ACP bridge exited before the protocol runtime completed with %s; %s",
					bridgeExitStatus(child.state), bridgeStdoutRule)
			}
		}
	}
	if driveDone != nil {
		// Rust drops the driver here; its connection and transport go with
		// it.
		stopDrive()
		releaseSessionRestart((<-driveDone).restart)
	}
	session, unlock := opened.Lock()
	openedNow := *session
	unlock()
	restarting := (result == nil && restart != nil) || (result != nil && openedNow != nil)
	// Dropping the transport closes the supervisor's stdin. Give it time to
	// terminate and reap the complete bridge process group before killing the
	// supervisor itself as a last resort. A planned restart already decided
	// to kill the child, so a non-zero exit is the expected outcome.
	stdin.Close()
	if !childReaped {
		if restarting {
			exited := child.wait(5 * time.Second)
			if exited && child.waitErr != nil && child.state == nil {
				slog.Warn("wait for retiring ACP supervisor", "error", child.waitErr)
				exited = false
			}
			if !exited {
				if err := child.command.Process.Kill(); err != nil && !errors.Is(err, os.ErrProcessDone) {
					slog.Warn("could not kill ACP bridge during planned restart",
						"operation", "acp_bridge_restart", "error", err)
				}
				child.wait(-1)
			}
		} else {
			var cleanup error
			if child.wait(5 * time.Second) {
				switch {
				case child.waitErr != nil && child.state == nil:
					cleanup = fmt.Errorf("wait for ACP bridge shutdown: %w", child.waitErr)
				case !child.state.Success():
					cleanup = fmt.Errorf("ACP bridge exited with %s after the protocol runtime completed",
						bridgeExitStatus(child.state))
				}
			} else {
				killErr := child.command.Process.Kill()
				if killErr != nil && errors.Is(killErr, os.ErrProcessDone) {
					killErr = nil
				}
				if killErr != nil {
					killErr = fmt.Errorf("kill unresponsive ACP bridge: %w", killErr)
				}
				child.wait(-1)
				cleanup = killErr
			}
			if cleanup != nil {
				mergeDriveError(&restart, &result, cleanup)
			}
		}
	}
	reporting := context.WithoutCancel(ctx)
	// The bridge lifetime owner also covers cancellation of drive before its
	// ordinary shutdown path; no terminal from this connection may outlive it.
	terminals.Shutdown(reporting, events)
	// Nor may its guardian reviewer: the next bridge prepares the same staged
	// home (OpenCode's login database included) only after this one is gone.
	if guardian != nil {
		guardian.shutdown(reporting)
	}
	if err := emitRuntimeEvent(reporting, events, core.RuntimeEventBridgeRetired{TerminalConnectionID: terminals.connectionID()}); err != nil {
		return nil, err
	}
	stderrResult := <-stderrDone
	tail := stderrResult.tail
	if stderrResult.err != nil {
		mergeDriveError(&restart, &result, stderrResult.err)
		tail = ""
	}
	if !restarting && result != nil {
		if actionable, ok := actionableStderrTail(tail); ok {
			result = fmt.Errorf("%s\n%s: %w", bridgeStderrContext, actionable, result)
		}
	}
	if result != nil {
		if openedNow == nil {
			return nil, result
		}
		// Carry the identity even if we can replace it. The next session open
		// decides whether to load or replace the thread.
		nativeSessionID := openedNow.nativeSessionID
		return &BridgeRestart{
			resumeSession:     &nativeSessionID,
			nativeSessionUsed: openedNow.nativeSessionUsed.Load(),
			sessionAge:        time.Since(openedNow.startedAt),
			notice:            BridgeRestartNoticeUnexpected{},
		}, nil
	}
	used := openedNow != nil && openedNow.nativeSessionUsed.Load()
	switch restart := restart.(type) {
	case nil:
		return nil, nil
	case SessionRestartClear:
		return &BridgeRestart{
			clearContext:      &restart,
			nativeSessionUsed: used,
			notice:            BridgeRestartNoticePlanned{Value: "Clearing context"},
		}, nil
	case SessionRestartReloadLaunchConfig:
		var age time.Duration
		if openedNow != nil {
			age = time.Since(openedNow.startedAt)
		}
		return &BridgeRestart{
			resumeSession: restart.ResumeSession,
			reloadLaunchConfig: &launchConfigReload{
				launchConfig:          restart.LaunchConfig,
				environment:           restart.Environment,
				command:               restart.Command,
				args:                  restart.Args,
				additionalDirectories: restart.AdditionalDirectories,
				supervisorSpec:        restart.SupervisorSpec,
				leases:                restart.Leases,
				resolved:              restart.Resolved,
			},
			nativeSessionUsed: used,
			sessionAge:        age,
			notice:            BridgeRestartNoticePlanned{Value: "Restarting harness under updated launch configuration"},
		}, nil
	case SessionRestartResume, SessionRestartReloadLogins:
		var nativeSessionID, message string
		switch restart := restart.(type) {
		case SessionRestartResume:
			nativeSessionID, message = restart.Value, acpBridgeRestartWarning
		case SessionRestartReloadLogins:
			nativeSessionID, message = restart.Value, opencodeLoginsReloadMessage
		}
		var age time.Duration
		if openedNow != nil {
			age = time.Since(openedNow.startedAt)
		}
		return &BridgeRestart{
			resumeSession:     &nativeSessionID,
			nativeSessionUsed: used,
			sessionAge:        age,
			notice:            BridgeRestartNoticePlanned{Value: message},
		}, nil
	}
	panic(fmt.Sprintf("unknown session restart %T", restart))
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
	var kept []string
	for _, line := range rustLines(tail) {
		trimmed := strings.TrimSpace(line)
		if !strings.HasPrefix(trimmed, adapterChatterPrefix) && trimmed != kimiNullResponseChatter {
			kept = append(kept, line)
		}
	}
	joined := strings.TrimSpace(strings.Join(kept, "\n"))
	if joined == "" {
		return "", false
	}
	return joined, true
}

// rustLines splits text as Rust's str::lines does: on `\n`, dropping one
// trailing `\r` from each line and the empty piece after a final newline.
func rustLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(text, "\n")
	if lines[len(lines)-1] == "" {
		lines = lines[:len(lines)-1]
	}
	for i, line := range lines {
		lines[i] = strings.TrimSuffix(line, "\r")
	}
	return lines
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
		releaseSessionRestart(*restart)
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

// preambleReader is the transport side of skipStdoutPreamble: closing it
// also closes the bridge's stdout, so the relay goroutine stops.
type preambleReader struct {
	*io.PipeReader
	stdout io.Reader
}

func (r preambleReader) Close() error {
	err := r.PipeReader.Close()
	if closer, ok := r.stdout.(io.Closer); ok {
		_ = closer.Close()
	}
	return err
}

// skipStdoutPreamble passes a bridge's stdout through to the ACP transport,
// dropping the non-JSON lines a launcher or login shell prints before the
// bridge's first JSON-RPC frame. Each dropped line is logged. The ACP
// transport answers every unparsable line with a parse error, and a Kimi
// launcher's installer output ended a session that way (#1136). From the
// first frame on, bytes pass through unchanged, so the transport stays
// strict. A bridge that never sends a frame still fails at the initialize
// timeout.
func skipStdoutPreamble(stdout io.Reader) io.ReadCloser {
	reader, writer := io.Pipe()
	go func() {
		defer writer.Close()
		lines := bufio.NewReaderSize(stdout, 64*1024)
		for {
			line, err := lines.ReadBytes('\n')
			if len(line) == 0 {
				if err != nil && !errors.Is(err, io.EOF) {
					slog.Warn("read ACP bridge stdout before its first frame", "error", err)
				}
				return
			}
			// Rust's trim_ascii: ASCII whitespace only.
			text := bytes.Trim(line, " \t\n\f\r")
			if json.Valid(text) {
				if _, writeErr := writer.Write(line); writeErr != nil {
					return
				}
				break
			}
			if len(text) > 0 {
				slog.Warn("skipping non-JSON ACP bridge output before its first JSON-RPC frame",
					"line", fromUTF8Lossy(text))
			}
			if err != nil {
				if !errors.Is(err, io.EOF) {
					slog.Warn("read ACP bridge stdout before its first frame", "error", err)
				}
				return
			}
		}
		if _, err := io.Copy(writer, lines); err != nil {
			slog.Debug("ACP bridge stdout relay ended", "error", err)
		}
	}()
	return preambleReader{PipeReader: reader, stdout: stdout}
}

func readStderrTail(stderr io.Reader) (string, error) {
	var tail []byte
	buffer := make([]byte, 4096)
	for {
		read, err := stderr.Read(buffer)
		if read > 0 {
			tail = append(tail, buffer[:read]...)
			if len(tail) > acpStderrTailBytes {
				tail = append(tail[:0], tail[len(tail)-acpStderrTailBytes:]...)
			}
		}
		if err != nil {
			if errors.Is(err, io.EOF) || errors.Is(err, os.ErrClosed) {
				break
			}
			return "", fmt.Errorf("read ACP bridge stderr: %s", acpIOError(err))
		}
	}
	return strings.TrimSpace(fromUTF8Lossy(tail)), nil
}

const acpBridgeLostWarning = "ACP bridge exited; reloading the native session"

const acpBridgeRestartWarning = "ACP bridge restarting; reloading the native session"

const opencodeLoginsReloadMessage = "The profile's OpenCode logins changed; reloading the session to use them"

// ResolveKimiSessionDir is kimi_tasks::resolve_session_dir, which acp.rs
// re-exports under this name.
func ResolveKimiSessionDir(kimiHome string, sessionID string) (string, error) {
	return ResolveSessionDir(kimiHome, sessionID)
}

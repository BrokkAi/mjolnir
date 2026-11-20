// Port of mj-core/src/acp.rs.

// Normalized ACP data and controls shared by workers and control surfaces.

package core

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"strings"

	"github.com/BrokkAi/acp-go/schema"
)

// PlanReviewIDPrefix is the identity prefix every normalized plan decision
// shares, whatever harness dialect produced it.
const PlanReviewIDPrefix = "plan-review-"

// PlanReviewMessagePrefix is the header NormalizedPlanReview puts in front of
// the harness's proposal text. Reading the proposal back out is the inverse,
// so both live here.
const PlanReviewMessagePrefix = "Review the agent's plan:\n\n"

const PlanReviewAction = "action"

const PlanReviewFeedback = "feedback"

// PromptAuthRequiredMarker is the marker Hel adds to the warning for a prompt
// the bridge failed with ACP's `auth_required`. The wire message is a bare
// "Authentication required", too generic for `credentials` to match on text
// alone, so the error code — not the bridge's wording — decides whether the
// credential heuristic fires.
const PromptAuthRequiredMarker = "ACP auth_required"

// PromptEmptyResponseMarker marks a successful ACP response that carried no
// session updates. Some bridges use this shape when their underlying turn
// failed, so completing it silently would leave a user line with no answer or
// explanation.
const PromptEmptyResponseMarker = "ACP prompt returned no session updates"

// SessionConfigRecoveryID is the identity of the one question Hel raises about
// a session's own stored configuration.
//
// It is constant because the projection dedupes pending elicitations by id:
// a restart before the operator answers replaces the question instead of
// stacking another copy of it.
const SessionConfigRecoveryID = "session-config-recovery"

func PlanReviewCarriesNativeFeedback(id string) bool {
	return GrokIsPlanReviewID(id)
}

// IsPlanReviewID reports whether this elicitation id belongs to one of Hel's
// normalized plan decisions.
func IsPlanReviewID(id string) bool {
	return strings.HasPrefix(id, PlanReviewIDPrefix)
}

// PlanReviewProposal returns the exact proposal text a normalized plan
// decision carries.
//
// Returns false for any other elicitation, and for a plan decision whose
// message was not built by NormalizedPlanReview.
func PlanReviewProposal(request *ElicitationRequest) (string, bool) {
	if !IsPlanReviewID(request.ID) {
		return "", false
	}
	return strings.CutPrefix(request.Message, PlanReviewMessagePrefix)
}

func PlanReviewKeepPlanning() ElicitationResponse {
	panic("unported: mj-core/src/acp.rs plan_review_keep_planning")
}

func NormalizedPlanReview(id string, value any) ElicitationRequest {
	panic("unported: mj-core/src/acp.rs normalized_plan_review")
}

// NestedString finds the first string under any of keys, searching value
// depth first. value is a decoded JSON value (map[string]any, []any, ...).
func NestedString(value any, keys []string) (string, bool) {
	panic("unported: mj-core/src/acp.rs nested_string")
}

// SessionConfigChoices returns every value the harness currently advertises
// for key, in advertised order and with option groups flattened.
//
// Empty when the harness advertises no such option or exposes it as something
// other than a select, which callers read as "not configurable".
func SessionConfigChoices(options []schema.SessionConfigOption, key string) []SessionConfigChoice {
	panic("unported: mj-core/src/acp.rs session_config_choices")
}

// MissingConfigSelectorRefusal says what to report when the harness
// advertises no setting for key at all. The worker, the terminal and the web
// viewer say the same words, so the refusal a surface shows the moment it
// knows and the one the transcript records agree. Names the harness when it
// is known: "DSH does not offer a reasoning effort setting".
func MissingConfigSelectorRefusal(harness *HarnessKind, key string) string {
	setting := key
	if key == "effort" {
		setting = "reasoning effort"
	}
	article := "a"
	if setting != "" && strings.ContainsRune("aeiou", rune(setting[0])) {
		article = "an"
	}
	subject := "This agent"
	if harness != nil {
		subject = harness.DisplayName()
	}
	return fmt.Sprintf("%s does not offer %s %s setting", subject, article, setting)
}

// EffortOptionIDs are the option ids harnesses use for the effort selector
// when they publish no thought-level category. The canonical key is `effort`.
var EffortOptionIDs = [2]string{"effort", "reasoning_effort"}

func FindSessionConfigOption(options []schema.SessionConfigOption, key string) *schema.SessionConfigOption {
	panic("unported: mj-core/src/acp.rs find_session_config_option")
}

// ExecutionModeOption returns the config option that offers desired: selector
// when the bridge lists it there, otherwise the bridge's Mode selector.
func ExecutionModeOption(configOptions []schema.SessionConfigOption, selector *string, desired string) *schema.SessionConfigOption {
	panic("unported: mj-core/src/acp.rs execution_mode_option")
}

// SelectContains reports whether the option is a select offering desired.
// The Rust signature takes the option's SessionConfigKind; acp-go keeps the
// kind on the option itself.
func SelectContains(option *schema.SessionConfigOption, desired string) bool {
	panic("unported: mj-core/src/acp.rs select_contains")
}

// SessionUpdateIsSessionState reports current session state, independent of
// replayed conversation history. Unknown future variants are history until
// explicitly classified otherwise. Usage is also history: replaying it could
// double-count an old turn.
func SessionUpdateIsSessionState(update *schema.SessionUpdate) bool {
	return update.AvailableCommandsUpdate != nil ||
		update.ConfigOptionUpdate != nil ||
		update.CurrentModeUpdate != nil ||
		update.SessionInfoUpdate != nil
}

// PromptUnansweredStopReason is the stop reason recorded for a turn the
// harness ended without answering.
//
// A reason of its own, like `harness_inactive` for a stalled turn, so
// `mj wait`, the session summary, the chat and the recorded events all name
// what happened and automation can tell this apart from any other error. Any
// stop reason that is not a known completion already classifies as an error
// (ClassifyPromptCompletion), so nothing has to learn this string to keep
// working.
const PromptUnansweredStopReason = "prompt_unanswered"

// AwaitingInputStopReason: the classifier inferred a user handoff while the
// harness still held its prompt open.
const AwaitingInputStopReason = "awaiting_input"

// InferredFinishedStopReason: the classifier inferred that a silent turn
// finished while the harness held it open.
const InferredFinishedStopReason = "inferred_finished"

// compactionBanners is the progress text an ACP bridge streams while it
// compacts a session's context, verbatim from the bridges Mjolnir pins.
//
// `@agentclientprotocol/claude-agent-acp` emits these as ordinary assistant
// text (`dist/acp-agent.js`, the `status` handler keyed on `compacting` and
// `compact_result`), so they are indistinguishable from an answer unless they
// are named. Compaction is not in the ACP schema this build compiles against,
// which is why there is nothing better to match on. See issue #970.
var compactionBanners = []string{
	"compacting...",
	"compacting completed.",
	"compacting failed",
}

// SessionUpdateIsCompactionBanner reports whether this update is a bridge's
// own compaction progress text rather than anything the agent produced by
// working.
//
// A turn carrying only these compacted the context; it did not act on the
// prompt. Recognizing them is what lets Mjolnir see a prompt that was
// swallowed during compaction, which is the loss reported in #970.
func SessionUpdateIsCompactionBanner(update *schema.SessionUpdate) bool {
	chunk := update.AgentMessageChunk
	if chunk == nil {
		chunk = update.AgentThoughtChunk
	}
	if chunk == nil || chunk.Content.Text == nil {
		return false
	}
	text := strings.ToLower(strings.TrimSpace(chunk.Content.Text.Text))
	for _, banner := range compactionBanners {
		if strings.HasPrefix(text, banner) {
			return true
		}
	}
	return false
}

// ContextCommand says whether this prompt is Mjolnir forwarding a request to
// clear or compact the context.
//
// A turn that answers this prompt with nothing but compaction banners did
// exactly what was asked, so it must not be reported as unanswered. Only
// Mjolnir's own outgoing text is inspected, never the harness's.
type ContextCommand int

const (
	ContextCommandClear ContextCommand = iota
	ContextCommandCompact
)

// ContextCommandOf interprets only the user's first block, before attaching
// background context. (Rust: `context_command`; the name is taken by the type.)
func ContextCommandOf(prompt []schema.ContentBlock) (ContextCommand, string, bool) {
	if len(prompt) == 0 || prompt[0].Text == nil {
		return 0, "", false
	}
	return ContextCommandText(prompt[0].Text.Text)
}

func ContextCommandText(text string) (ContextCommand, string, bool) {
	panic("unported: mj-core/src/acp.rs context_command_text")
}

func PromptRequestsCompaction(prompt []schema.ContentBlock) bool {
	command, _, ok := ContextCommandOf(prompt)
	return ok && command == ContextCommandCompact
}

// PromptIsSlashCommand reports whether the prompt is a harness slash command
// such as `/review` rather than a message. The command name is one word of
// letters, digits, `_`, `-` or `:`, so a prompt that opens with a path like
// `/home/me/file` is a message.
func PromptIsSlashCommand(prompt []schema.ContentBlock) bool {
	panic("unported: mj-core/src/acp.rs prompt_is_slash_command")
}

// SteeringTextContext returns prompt text that can be represented faithfully
// as one plain text context block at a harness tool boundary.
func SteeringTextContext(prompt []schema.ContentBlock) (string, bool) {
	if len(prompt) != 1 || prompt[0].Text == nil {
		return "", false
	}
	if _, _, isContext := ContextCommandOf(prompt); PromptIsSlashCommand(prompt) || isContext {
		return "", false
	}
	return prompt[0].Text.Text, true
}

// SessionUpdateIsAgentOutput reports whether this update is the agent doing
// the work a prompt asked for.
//
// This is how Mjolnir tells "the harness answered" from "the harness ended
// the turn without acting on the prompt" (#970). Answering is deliberately
// defined widely: a turn that only ran tools, edited files or executed
// commands and never wrote a word has answered. Only traffic that arrives
// without the agent having done anything is excluded — command catalogues,
// mode and configuration announcements, session metadata, and usage
// accounting, all of which a harness emits on its own schedule.
//
// An unrecognized future variant counts as output. SessionUpdate is
// non-exhaustive, and reporting an answered turn as unanswered because this
// build is older than the harness would be worse than missing a swallow.
func SessionUpdateIsAgentOutput(update *schema.SessionUpdate) bool {
	return !SessionUpdateIsSessionState(update) &&
		update.UsageUpdate == nil &&
		!SessionUpdateIsCompactionBanner(update)
}

// CodexMissingThreadMessage is the stable part of Codex's refusal to resume a
// thread it never wrote to disk. Codex defers a thread's rollout file until
// the first user message, so a thread that was created and never prompted
// does not exist to resume. codex-acp wraps the message, and it reaches
// Mjolnir looking like
// `Internal error: {"details": "no rollout found for thread id 0199…"}`, so
// the substring is what can be matched.
const CodexMissingThreadMessage = "no rollout found for thread id"

// CodexThreadNotFoundMessage: newer codex-acp builds report the same
// never-written thread as `thread not found: <thread id>`. It names the
// thread, so it only counts for the thread being resumed.
const CodexThreadNotFoundMessage = "thread not found: "

// ErrorReportsMissingNativeSession reports whether a failed `session/resume`
// or `session/load` says the harness has no such native session. Only
// harnesses that defer writing a session to disk until its first user message
// can report a session Mjolnir believes it created; every other harness
// answers false, so its reload failure keeps failing loudly. If a harness
// changes its answer, this stops matching and the resume fails loudly again;
// it can never degrade into silently replacing a native session that still
// exists.
func ErrorReportsMissingNativeSession(harness HarnessKind, nativeSessionID string, err error) bool {
	panic("unported: mj-core/src/acp.rs error_reports_missing_native_session")
}

// AcceptedSessionConfig: only model and reasoning effort survive a bridge
// replacement. Restoring plan/permission modes here could override the
// current execution policy.
type AcceptedSessionConfig struct {
	Model  *string
	Effort *string
}

// ClaudeBackgroundTask is a historical raw SDK task record, retained for old
// RuntimeEvent payloads.
type ClaudeBackgroundTask struct {
	TaskID      string `json:"task_id"`
	Description string `json:"description"`
}

// AirTaskUpdate is a change in the adapter-owned AIR background task
// inventory. Externally tagged on the wire: {"TurnSpawned": {...}}.
type AirTaskUpdate interface {
	isAirTaskUpdate()
}

type AirTaskUpdateTurnSpawned struct {
	TaskID   string `json:"task_id"`
	Replayed bool   `json:"replayed"`
}

type AirTaskUpdateSpawned struct {
	TaskID      string `json:"task_id"`
	Description string `json:"description"`
	CanStop     bool   `json:"can_stop"`
}

type AirTaskUpdateChanged struct {
	TaskID      string  `json:"task_id"`
	Description *string `json:"description"`
	CanStop     *bool   `json:"can_stop"`
}

type AirTaskUpdateFinished struct {
	TaskID  string `json:"task_id"`
	Settled bool   `json:"settled"`
	Failed  bool   `json:"failed"`
}

func (AirTaskUpdateTurnSpawned) isAirTaskUpdate() {}
func (AirTaskUpdateSpawned) isAirTaskUpdate()     {}
func (AirTaskUpdateChanged) isAirTaskUpdate()     {}
func (AirTaskUpdateFinished) isAirTaskUpdate()    {}

func (v AirTaskUpdateTurnSpawned) MarshalJSON() ([]byte, error) {
	type body AirTaskUpdateTurnSpawned
	return agentExternalJSON("TurnSpawned", body(v))
}

func (v AirTaskUpdateSpawned) MarshalJSON() ([]byte, error) {
	type body AirTaskUpdateSpawned
	return agentExternalJSON("Spawned", body(v))
}

func (v AirTaskUpdateChanged) MarshalJSON() ([]byte, error) {
	type body AirTaskUpdateChanged
	return agentExternalJSON("Changed", body(v))
}

func (v AirTaskUpdateFinished) MarshalJSON() ([]byte, error) {
	type body AirTaskUpdateFinished
	return agentExternalJSON("Finished", body(v))
}

// UnmarshalAirTaskUpdate decodes an externally tagged AirTaskUpdate.
func UnmarshalAirTaskUpdate(data []byte) (AirTaskUpdate, error) {
	name, body, err := agentExternalVariant(data)
	if err != nil {
		return nil, err
	}
	switch name {
	case "TurnSpawned":
		return agentDecodeInto[AirTaskUpdateTurnSpawned, AirTaskUpdate](body)
	case "Spawned":
		return agentDecodeInto[AirTaskUpdateSpawned, AirTaskUpdate](body)
	case "Changed":
		return agentDecodeInto[AirTaskUpdateChanged, AirTaskUpdate](body)
	case "Finished":
		return agentDecodeInto[AirTaskUpdateFinished, AirTaskUpdate](body)
	}
	return nil, fmt.Errorf("unknown variant `%s`, expected one of `TurnSpawned`, `Spawned`, `Changed`, `Finished`", name)
}

// RuntimeEvent is internally tagged on the wire by "type" in snake_case.
type RuntimeEvent interface {
	isRuntimeEvent()
}

// RuntimeEventBridgeRetired is a worker-local lifetime message, never a
// persisted adapter event (serde skip: it neither encodes nor decodes).
type RuntimeEventBridgeRetired struct {
	TerminalConnectionID string
}

// RuntimeEventNativeGoalResumeSent is ordered before the native request and
// its updates on the worker FIFO.
type RuntimeEventNativeGoalResumeSent struct {
	ResumeID uint64        `json:"resume_id"`
	Goal     *GoalSnapshot `json:"goal"`
}

type RuntimeEventNativeGoalResumeRejected struct {
	ResumeID uint64 `json:"resume_id"`
}

type RuntimeEventNativeAgent struct {
	Event NativeAgentEvent `json:"event"`
}

type RuntimeEventConnected struct {
	AgentName         *string                   `json:"agent_name"`
	AgentVersion      *string                   `json:"agent_version"`
	ProtocolVersion   *schema.ProtocolVersion   `json:"protocol_version,omitempty"`
	Capabilities      *schema.AgentCapabilities `json:"capabilities,omitempty"`
	AgentInfo         *schema.Implementation    `json:"agent_info,omitempty"`
	SteeringSupported *bool                     `json:"steering_supported,omitempty"`
}

type RuntimeEventContextClearing struct {
	RequestID string `json:"request_id"`
}

type RuntimeEventContextCleared struct {
	RequestID       string  `json:"request_id"`
	NativeSessionID string  `json:"native_session_id"`
	Memory          *string `json:"memory"`
}

type RuntimeEventSessionStarted struct {
	NativeSessionID string  `json:"native_session_id"`
	Resumed         bool    `json:"resumed"`
	ExecutionMode   *string `json:"execution_mode,omitempty"`
	// NativeContinuityLost: this session opened fresh because the recorded
	// native session could not be reloaded and the harness keeps no native
	// state a checkpoint could restore. Older workers omit it.
	NativeContinuityLost bool `json:"native_continuity_lost"`
	// ReplacedUnusedNativeSessionID is the recorded native session this one
	// replaced because the harness had no record of it and this session
	// never used it.
	ReplacedUnusedNativeSessionID *string `json:"replaced_unused_native_session_id,omitempty"`
}

type RuntimeEventSessionConfigured struct {
	ConfigOptions []schema.SessionConfigOption `json:"config_options"`
}

// RuntimeEventNativeSessionUsed: the native thread now holds something only
// it can replay: the agent sent conversation content, or a prompt was
// transmitted to it. The worker persists this so a later restart never treats
// the thread as an empty one it may replace. Emitted once per bridge life, on
// the change.
type RuntimeEventNativeSessionUsed struct{}

type RuntimeEventSessionModesConfigured struct {
	Modes *schema.SessionModeState `json:"modes"`
}

type RuntimeEventSessionUpdate struct {
	Update json.RawMessage `json:"update"`
}

// RuntimeEventClaudeBackgroundTasksChanged is a historical runtime payload;
// retained only for transcript decoding.
type RuntimeEventClaudeBackgroundTasksChanged struct {
	Tasks []ClaudeBackgroundTask `json:"tasks"`
}

// RuntimeEventClaudeAsyncTaskControlChanged is a historical runtime payload;
// retained only for transcript decoding.
type RuntimeEventClaudeAsyncTaskControlChanged struct {
	TaskID  string `json:"task_id"`
	CanStop bool   `json:"can_stop"`
	// AutonomousTurn: claude-agent-acp 0.97.0 publishes autonomous model
	// cycles through the async task API; these are not background work or a
	// pending task turn.
	AutonomousTurn bool `json:"autonomous_turn"`
	// Replayed: the edge arrived before live history was enabled.
	Replayed bool `json:"replayed"`
	// Settled: an ordinary task completed or failed, so Claude Code will
	// follow it with a task-notification turn. A stopped task gets no turn.
	Settled bool `json:"settled"`
}

type RuntimeEventAirTaskChanged struct {
	SessionID string        `json:"session_id"`
	Update    AirTaskUpdate `json:"update"`
}

func (v *RuntimeEventAirTaskChanged) UnmarshalJSON(data []byte) error {
	var raw struct {
		SessionID string          `json:"session_id"`
		Update    json.RawMessage `json:"update"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	update, err := UnmarshalAirTaskUpdate(raw.Update)
	if err != nil {
		return err
	}
	*v = RuntimeEventAirTaskChanged{SessionID: raw.SessionID, Update: update}
	return nil
}

// RuntimeEventClaudeTurnResult: Claude Code reported the end of one model
// cycle. It travels on the same ordered stream as SessionUpdate, so
// everything the adapter sent before the result is recorded before it. The
// result's fields sit beside the tag.
type RuntimeEventClaudeTurnResult struct {
	ClaudeTurnResult
}

type RuntimeEventElicitationRequested struct {
	Request ElicitationRequest `json:"request"`
}

type RuntimeEventElicitationResolved struct {
	ElicitationID string  `json:"elicitation_id"`
	Action        string  `json:"action"`
	Reply         *string `json:"reply,omitempty"`
}

type RuntimeEventPromptFinished struct {
	Diagnostic *TurnDiagnostic `json:"diagnostic,omitempty"`
	RequestID  string          `json:"request_id,omitempty"`
	StopReason string          `json:"stop_reason"`
	Usage      *TokenUsage     `json:"usage,omitempty"`
}

type RuntimeEventNotice struct {
	Message string `json:"message"`
}

type RuntimeEventSessionFault struct {
	Message string `json:"message"`
}

type RuntimeEventWarning struct {
	Message string `json:"message"`
}

// RuntimeEventTerminalStarted is a terminal's ordered fallback card, queued
// before process spawn. The worker records an interim tool call so agents
// that omit the ACP association do not strand its eventual result as a
// standalone transcript item.
type RuntimeEventTerminalStarted struct {
	TerminalID  string `json:"terminal_id"`
	Command     string `json:"command"`
	StartedAtMs int64  `json:"started_at_ms"`
}

// RuntimeEventTerminalStartFailed: spawn failed after its ordered start
// marker; release the inventory only when the owner applies this event, after
// writing the failed card.
type RuntimeEventTerminalStartFailed struct {
	TerminalID string `json:"terminal_id"`
}

// RuntimeEventTerminalClosed: a client-run terminal was reaped. Exactly one
// of these is emitted per terminal, by the supervisor that waits on the
// child, so kill and release flow through the same report.
type RuntimeEventTerminalClosed struct {
	TerminalID string  `json:"terminal_id"`
	Output     string  `json:"output"`
	Truncated  bool    `json:"truncated"`
	ExitCode   *uint32 `json:"exit_code,omitempty"`
	Signal     *string `json:"signal,omitempty"`
}

type RuntimeEventUserShellOutput struct {
	RequestID       string `json:"request_id"`
	Command         string `json:"command"`
	Stdout          string `json:"stdout"`
	Stderr          string `json:"stderr"`
	StdoutTruncated bool   `json:"stdout_truncated"`
	StderrTruncated bool   `json:"stderr_truncated"`
}

type RuntimeEventUserShellFinished struct {
	RequestID string          `json:"request_id"`
	Result    UserShellResult `json:"result"`
}

type RuntimeEventConfigApplied struct {
	RequestID string `json:"request_id,omitempty"`
	Key       string `json:"key"`
	Value     string `json:"value"`
	// ConfigOptions is the complete configuration returned by ACP for this
	// change. Keep it in the same runtime event as command completion so the
	// relay cannot publish a checkpoint between the two durable observations.
	ConfigOptions []schema.SessionConfigOption `json:"config_options"`
}

type RuntimeEventGoalControlApplied struct {
	RequestID string `json:"request_id"`
}

type RuntimeEventSessionModeApplied struct {
	RequestID     string                       `json:"request_id,omitempty"`
	ModeID        string                       `json:"mode_id"`
	ConfigOptions []schema.SessionConfigOption `json:"config_options"`
	Modes         *schema.SessionModeState     `json:"modes,omitempty"`
}

type RuntimeEventCommandRejected struct {
	Reason    OutcomeReason `json:"reason"`
	RequestID string        `json:"request_id"`
	Message   string        `json:"message"`
}

type RuntimeEventCommandInterrupted struct {
	Reason    OutcomeReason `json:"reason"`
	RequestID string        `json:"request_id"`
	Message   string        `json:"message"`
}

type RuntimeEventCancelApplied struct {
	RequestID string `json:"request_id"`
}

type RuntimeEventSteeringUnconfirmed struct {
	RequestID string `json:"request_id"`
	Message   string `json:"message"`
}

type RuntimeEventSteerApplied struct {
	RequestID       string `json:"request_id"`
	QueuedCommandID string `json:"queued_command_id"`
}

// RuntimeEventSteerReturned: the bridge had no running turn for the steer and
// returned its prompt.
type RuntimeEventSteerReturned struct {
	RequestID       string `json:"request_id"`
	QueuedCommandID string `json:"queued_command_id"`
}

type RuntimeEventCloseApplied struct {
	RequestID string `json:"request_id"`
}

// RuntimeEventHarnessRestarting: the ACP child died or the protocol broke
// after a session was open. The coordinator interrupts in-flight commands;
// the runtime reloads the native session on a new bridge instead of stopping
// the worker.
type RuntimeEventHarnessRestarting struct {
	Message string `json:"message"`
}

type RuntimeEventStopped struct{}

func (RuntimeEventBridgeRetired) isRuntimeEvent()                 {}
func (RuntimeEventNativeGoalResumeSent) isRuntimeEvent()          {}
func (RuntimeEventNativeGoalResumeRejected) isRuntimeEvent()      {}
func (RuntimeEventNativeAgent) isRuntimeEvent()                   {}
func (RuntimeEventConnected) isRuntimeEvent()                     {}
func (RuntimeEventContextClearing) isRuntimeEvent()               {}
func (RuntimeEventContextCleared) isRuntimeEvent()                {}
func (RuntimeEventSessionStarted) isRuntimeEvent()                {}
func (RuntimeEventSessionConfigured) isRuntimeEvent()             {}
func (RuntimeEventNativeSessionUsed) isRuntimeEvent()             {}
func (RuntimeEventSessionModesConfigured) isRuntimeEvent()        {}
func (RuntimeEventSessionUpdate) isRuntimeEvent()                 {}
func (RuntimeEventClaudeBackgroundTasksChanged) isRuntimeEvent()  {}
func (RuntimeEventClaudeAsyncTaskControlChanged) isRuntimeEvent() {}
func (RuntimeEventAirTaskChanged) isRuntimeEvent()                {}
func (RuntimeEventClaudeTurnResult) isRuntimeEvent()              {}
func (RuntimeEventElicitationRequested) isRuntimeEvent()          {}
func (RuntimeEventElicitationResolved) isRuntimeEvent()           {}
func (RuntimeEventPromptFinished) isRuntimeEvent()                {}
func (RuntimeEventNotice) isRuntimeEvent()                        {}
func (RuntimeEventSessionFault) isRuntimeEvent()                  {}
func (RuntimeEventWarning) isRuntimeEvent()                       {}
func (RuntimeEventTerminalStarted) isRuntimeEvent()               {}
func (RuntimeEventTerminalStartFailed) isRuntimeEvent()           {}
func (RuntimeEventTerminalClosed) isRuntimeEvent()                {}
func (RuntimeEventUserShellOutput) isRuntimeEvent()               {}
func (RuntimeEventUserShellFinished) isRuntimeEvent()             {}
func (RuntimeEventConfigApplied) isRuntimeEvent()                 {}
func (RuntimeEventGoalControlApplied) isRuntimeEvent()            {}
func (RuntimeEventSessionModeApplied) isRuntimeEvent()            {}
func (RuntimeEventCommandRejected) isRuntimeEvent()               {}
func (RuntimeEventCommandInterrupted) isRuntimeEvent()            {}
func (RuntimeEventCancelApplied) isRuntimeEvent()                 {}
func (RuntimeEventSteeringUnconfirmed) isRuntimeEvent()           {}
func (RuntimeEventSteerApplied) isRuntimeEvent()                  {}
func (RuntimeEventSteerReturned) isRuntimeEvent()                 {}
func (RuntimeEventCloseApplied) isRuntimeEvent()                  {}
func (RuntimeEventHarnessRestarting) isRuntimeEvent()             {}
func (RuntimeEventStopped) isRuntimeEvent()                       {}

func (RuntimeEventBridgeRetired) MarshalJSON() ([]byte, error) {
	return nil, errors.New("the enum variant RuntimeEvent::BridgeRetired cannot be serialized")
}

func (v RuntimeEventNativeGoalResumeSent) MarshalJSON() ([]byte, error) {
	type body RuntimeEventNativeGoalResumeSent
	return agentTaggedJSON("type", "native_goal_resume_sent", body(v))
}

func (v RuntimeEventNativeGoalResumeRejected) MarshalJSON() ([]byte, error) {
	type body RuntimeEventNativeGoalResumeRejected
	return agentTaggedJSON("type", "native_goal_resume_rejected", body(v))
}

func (v RuntimeEventNativeAgent) MarshalJSON() ([]byte, error) {
	type body RuntimeEventNativeAgent
	return agentTaggedJSON("type", "native_agent", body(v))
}

func (v RuntimeEventConnected) MarshalJSON() ([]byte, error) {
	type body RuntimeEventConnected
	return agentTaggedJSON("type", "connected", body(v))
}

func (v RuntimeEventContextClearing) MarshalJSON() ([]byte, error) {
	type body RuntimeEventContextClearing
	return agentTaggedJSON("type", "context_clearing", body(v))
}

func (v RuntimeEventContextCleared) MarshalJSON() ([]byte, error) {
	type body RuntimeEventContextCleared
	return agentTaggedJSON("type", "context_cleared", body(v))
}

func (v RuntimeEventSessionStarted) MarshalJSON() ([]byte, error) {
	type body RuntimeEventSessionStarted
	return agentTaggedJSON("type", "session_started", body(v))
}

func (v RuntimeEventSessionConfigured) MarshalJSON() ([]byte, error) {
	type body RuntimeEventSessionConfigured
	return agentTaggedJSON("type", "session_configured", body(v))
}

func (RuntimeEventNativeSessionUsed) MarshalJSON() ([]byte, error) {
	return agentTaggedJSON("type", "native_session_used", nil)
}

func (v RuntimeEventSessionModesConfigured) MarshalJSON() ([]byte, error) {
	type body RuntimeEventSessionModesConfigured
	return agentTaggedJSON("type", "session_modes_configured", body(v))
}

func (v RuntimeEventSessionUpdate) MarshalJSON() ([]byte, error) {
	type body RuntimeEventSessionUpdate
	return agentTaggedJSON("type", "session_update", body(v))
}

func (v RuntimeEventClaudeBackgroundTasksChanged) MarshalJSON() ([]byte, error) {
	type body RuntimeEventClaudeBackgroundTasksChanged
	return agentTaggedJSON("type", "claude_background_tasks_changed", body(v))
}

func (v RuntimeEventClaudeAsyncTaskControlChanged) MarshalJSON() ([]byte, error) {
	type body RuntimeEventClaudeAsyncTaskControlChanged
	return agentTaggedJSON("type", "claude_async_task_control_changed", body(v))
}

func (v RuntimeEventAirTaskChanged) MarshalJSON() ([]byte, error) {
	type body struct {
		SessionID string        `json:"session_id"`
		Update    AirTaskUpdate `json:"update"`
	}
	return agentTaggedJSON("type", "air_task_changed", body(v))
}

func (v RuntimeEventClaudeTurnResult) MarshalJSON() ([]byte, error) {
	return agentTaggedJSON("type", "claude_turn_result", v.ClaudeTurnResult)
}

func (v RuntimeEventElicitationRequested) MarshalJSON() ([]byte, error) {
	type body RuntimeEventElicitationRequested
	return agentTaggedJSON("type", "elicitation_requested", body(v))
}

func (v RuntimeEventElicitationResolved) MarshalJSON() ([]byte, error) {
	type body RuntimeEventElicitationResolved
	return agentTaggedJSON("type", "elicitation_resolved", body(v))
}

func (v RuntimeEventPromptFinished) MarshalJSON() ([]byte, error) {
	type body RuntimeEventPromptFinished
	return agentTaggedJSON("type", "prompt_finished", body(v))
}

func (v RuntimeEventNotice) MarshalJSON() ([]byte, error) {
	type body RuntimeEventNotice
	return agentTaggedJSON("type", "notice", body(v))
}

func (v RuntimeEventSessionFault) MarshalJSON() ([]byte, error) {
	type body RuntimeEventSessionFault
	return agentTaggedJSON("type", "session_fault", body(v))
}

func (v RuntimeEventWarning) MarshalJSON() ([]byte, error) {
	type body RuntimeEventWarning
	return agentTaggedJSON("type", "warning", body(v))
}

func (v RuntimeEventTerminalStarted) MarshalJSON() ([]byte, error) {
	type body RuntimeEventTerminalStarted
	return agentTaggedJSON("type", "terminal_started", body(v))
}

func (v RuntimeEventTerminalStartFailed) MarshalJSON() ([]byte, error) {
	type body RuntimeEventTerminalStartFailed
	return agentTaggedJSON("type", "terminal_start_failed", body(v))
}

func (v RuntimeEventTerminalClosed) MarshalJSON() ([]byte, error) {
	type body RuntimeEventTerminalClosed
	return agentTaggedJSON("type", "terminal_closed", body(v))
}

func (v RuntimeEventUserShellOutput) MarshalJSON() ([]byte, error) {
	type body RuntimeEventUserShellOutput
	return agentTaggedJSON("type", "user_shell_output", body(v))
}

func (v RuntimeEventUserShellFinished) MarshalJSON() ([]byte, error) {
	type body RuntimeEventUserShellFinished
	return agentTaggedJSON("type", "user_shell_finished", body(v))
}

func (v RuntimeEventConfigApplied) MarshalJSON() ([]byte, error) {
	type body RuntimeEventConfigApplied
	return agentTaggedJSON("type", "config_applied", body(v))
}

func (v RuntimeEventGoalControlApplied) MarshalJSON() ([]byte, error) {
	type body RuntimeEventGoalControlApplied
	return agentTaggedJSON("type", "goal_control_applied", body(v))
}

func (v RuntimeEventSessionModeApplied) MarshalJSON() ([]byte, error) {
	type body RuntimeEventSessionModeApplied
	return agentTaggedJSON("type", "session_mode_applied", body(v))
}

func (v RuntimeEventCommandRejected) MarshalJSON() ([]byte, error) {
	type body RuntimeEventCommandRejected
	return agentTaggedJSON("type", "command_rejected", body(v))
}

func (v RuntimeEventCommandInterrupted) MarshalJSON() ([]byte, error) {
	type body RuntimeEventCommandInterrupted
	return agentTaggedJSON("type", "command_interrupted", body(v))
}

func (v RuntimeEventCancelApplied) MarshalJSON() ([]byte, error) {
	type body RuntimeEventCancelApplied
	return agentTaggedJSON("type", "cancel_applied", body(v))
}

func (v RuntimeEventSteeringUnconfirmed) MarshalJSON() ([]byte, error) {
	type body RuntimeEventSteeringUnconfirmed
	return agentTaggedJSON("type", "steering_unconfirmed", body(v))
}

func (v RuntimeEventSteerApplied) MarshalJSON() ([]byte, error) {
	type body RuntimeEventSteerApplied
	return agentTaggedJSON("type", "steer_applied", body(v))
}

func (v RuntimeEventSteerReturned) MarshalJSON() ([]byte, error) {
	type body RuntimeEventSteerReturned
	return agentTaggedJSON("type", "steer_returned", body(v))
}

func (v RuntimeEventCloseApplied) MarshalJSON() ([]byte, error) {
	type body RuntimeEventCloseApplied
	return agentTaggedJSON("type", "close_applied", body(v))
}

func (v RuntimeEventHarnessRestarting) MarshalJSON() ([]byte, error) {
	type body RuntimeEventHarnessRestarting
	return agentTaggedJSON("type", "harness_restarting", body(v))
}

func (RuntimeEventStopped) MarshalJSON() ([]byte, error) {
	return agentTaggedJSON("type", "stopped", nil)
}

// UnmarshalRuntimeEvent decodes a RuntimeEvent by its "type" tag.
func UnmarshalRuntimeEvent(data []byte) (RuntimeEvent, error) {
	tag, err := agentTagOf(data, "type")
	if err != nil {
		return nil, err
	}
	switch tag {
	case "native_goal_resume_sent":
		return agentDecodeInto[RuntimeEventNativeGoalResumeSent, RuntimeEvent](data)
	case "native_goal_resume_rejected":
		return agentDecodeInto[RuntimeEventNativeGoalResumeRejected, RuntimeEvent](data)
	case "native_agent":
		return agentDecodeInto[RuntimeEventNativeAgent, RuntimeEvent](data)
	case "connected":
		return agentDecodeInto[RuntimeEventConnected, RuntimeEvent](data)
	case "context_clearing":
		return agentDecodeInto[RuntimeEventContextClearing, RuntimeEvent](data)
	case "context_cleared":
		return agentDecodeInto[RuntimeEventContextCleared, RuntimeEvent](data)
	case "session_started":
		return agentDecodeInto[RuntimeEventSessionStarted, RuntimeEvent](data)
	case "session_configured":
		return agentDecodeInto[RuntimeEventSessionConfigured, RuntimeEvent](data)
	case "native_session_used":
		return RuntimeEventNativeSessionUsed{}, nil
	case "session_modes_configured":
		return agentDecodeInto[RuntimeEventSessionModesConfigured, RuntimeEvent](data)
	case "session_update":
		return agentDecodeInto[RuntimeEventSessionUpdate, RuntimeEvent](data)
	case "claude_background_tasks_changed":
		return agentDecodeInto[RuntimeEventClaudeBackgroundTasksChanged, RuntimeEvent](data)
	case "claude_async_task_control_changed":
		return agentDecodeInto[RuntimeEventClaudeAsyncTaskControlChanged, RuntimeEvent](data)
	case "air_task_changed":
		return agentDecodeInto[RuntimeEventAirTaskChanged, RuntimeEvent](data)
	case "claude_turn_result":
		return agentDecodeInto[RuntimeEventClaudeTurnResult, RuntimeEvent](data)
	case "elicitation_requested":
		return agentDecodeInto[RuntimeEventElicitationRequested, RuntimeEvent](data)
	case "elicitation_resolved":
		return agentDecodeInto[RuntimeEventElicitationResolved, RuntimeEvent](data)
	case "prompt_finished":
		return agentDecodeInto[RuntimeEventPromptFinished, RuntimeEvent](data)
	case "notice":
		return agentDecodeInto[RuntimeEventNotice, RuntimeEvent](data)
	case "session_fault":
		return agentDecodeInto[RuntimeEventSessionFault, RuntimeEvent](data)
	case "warning":
		return agentDecodeInto[RuntimeEventWarning, RuntimeEvent](data)
	case "terminal_started":
		return agentDecodeInto[RuntimeEventTerminalStarted, RuntimeEvent](data)
	case "terminal_start_failed":
		return agentDecodeInto[RuntimeEventTerminalStartFailed, RuntimeEvent](data)
	case "terminal_closed":
		return agentDecodeInto[RuntimeEventTerminalClosed, RuntimeEvent](data)
	case "user_shell_output":
		return agentDecodeInto[RuntimeEventUserShellOutput, RuntimeEvent](data)
	case "user_shell_finished":
		return agentDecodeInto[RuntimeEventUserShellFinished, RuntimeEvent](data)
	case "config_applied":
		return agentDecodeInto[RuntimeEventConfigApplied, RuntimeEvent](data)
	case "goal_control_applied":
		return agentDecodeInto[RuntimeEventGoalControlApplied, RuntimeEvent](data)
	case "session_mode_applied":
		return agentDecodeInto[RuntimeEventSessionModeApplied, RuntimeEvent](data)
	case "command_rejected":
		return agentDecodeInto[RuntimeEventCommandRejected, RuntimeEvent](data)
	case "command_interrupted":
		return agentDecodeInto[RuntimeEventCommandInterrupted, RuntimeEvent](data)
	case "cancel_applied":
		return agentDecodeInto[RuntimeEventCancelApplied, RuntimeEvent](data)
	case "steering_unconfirmed":
		return agentDecodeInto[RuntimeEventSteeringUnconfirmed, RuntimeEvent](data)
	case "steer_applied":
		return agentDecodeInto[RuntimeEventSteerApplied, RuntimeEvent](data)
	case "steer_returned":
		return agentDecodeInto[RuntimeEventSteerReturned, RuntimeEvent](data)
	case "close_applied":
		return agentDecodeInto[RuntimeEventCloseApplied, RuntimeEvent](data)
	case "harness_restarting":
		return agentDecodeInto[RuntimeEventHarnessRestarting, RuntimeEvent](data)
	case "stopped":
		return RuntimeEventStopped{}, nil
	}
	return nil, fmt.Errorf("unknown variant `%s`", tag)
}

// SessionConfigChoice is one selectable value of a session configuration
// option, flattened out of the harness's ACP select shape.
type SessionConfigChoice struct {
	Value       string  `json:"value"`
	Name        string  `json:"name"`
	Description *string `json:"description"`
}

// AcpSessionFacts is what one live session's ACP surface offers, for callers
// outside the chat.
//
// The phone server and the terminal must agree on which harness drives plan
// mode through a session mode and which drives it through a configuration
// key, on whether fast mode is available, and on which values a setting
// accepts. Those are facts about the harness rather than about the client, so
// they are answered here once instead of being decided again in each surface.
type AcpSessionFacts struct {
	surface AcpSessionSurface
}

func AcceptedSessionConfigFromConfiguration(values map[string]string, options []schema.SessionConfigOption) AcceptedSessionConfig {
	panic("unported: mj-core/src/acp.rs AcceptedSessionConfig::from_configuration")
}

// AcceptedSessionConfigFromArchivedValues returns the accepted pair as an
// archived session stored it, keyed by the canonical names. The archive keeps
// each value under the harness's own option id (Codex publishes effort as
// `reasoning_effort`) and no option catalogue, so the known effort ids are
// read here. values holds decoded JSON values.
func AcceptedSessionConfigFromArchivedValues(values map[string]any) map[string]string {
	panic("unported: mj-core/src/acp.rs AcceptedSessionConfig::from_archived_values")
}

func (c *AcceptedSessionConfig) Remember(key, value string, options []schema.SessionConfigOption) bool {
	panic("unported: mj-core/src/acp.rs AcceptedSessionConfig::remember")
}

// AcceptedSessionConfigRecordCompleted folds a completed selector command
// into the durable configuration using the same accepted pair as the live
// bridge. Startup advertisements alone must never replace it with the
// provider's defaults.
func AcceptedSessionConfigRecordCompleted(values map[string]string, key, value string, options []schema.SessionConfigOption) {
	panic("unported: mj-core/src/acp.rs AcceptedSessionConfig::record_completed")
}

// AcpSessionFactsFromOperational reads the facts out of one relay
// operational snapshot.
func AcpSessionFactsFromOperational(harnessKind HarnessKind, configuration map[string]string, configOptions []schema.SessionConfigOption, modes *schema.SessionModeState) AcpSessionFacts {
	panic("unported: mj-core/src/acp.rs AcpSessionFacts::from_operational")
}

func (f *AcpSessionFacts) SupportsPlanMode() bool {
	return f.surface.SupportsPlanMode()
}

func (f *AcpSessionFacts) PlanModeActive() bool {
	return f.surface.PlanModeActive()
}

func (f *AcpSessionFacts) SupportsFastMode() bool {
	return f.surface.SupportsFastMode()
}

func (f *AcpSessionFacts) FastModeActive() bool {
	return f.surface.FastModeActive()
}

func (f *AcpSessionFacts) CurrentModel() (string, bool) {
	return f.surface.CurrentModel()
}

func (f *AcpSessionFacts) CurrentEffort() (string, bool) {
	return f.surface.CurrentEffort()
}

// PlanControl returns the ACP call that turns plan mode on or off, or a
// sentence saying why this harness cannot.
func (f *AcpSessionFacts) PlanControl(active bool) (PlanControl, error) {
	control, failure := f.surface.PlanControl(active)
	if failure == nil {
		return control, nil
	}
	switch *failure {
	case PlanControlErrorCodexIncompatible:
		return nil, errors.New("This Codex ACP version does not expose collaboration_mode with plan/default values.")
	case PlanControlErrorGrokIncompatible:
		return nil, errors.New("This Grok Build version does not expose compatible plan/default modes.")
	}
	return nil, errors.New("This ACP harness does not expose compatible plan/default modes.")
}

// PlanReviewAnswer reads the action and the optional revision feedback out of
// a plan review response.
func PlanReviewAnswer(response ElicitationResponse) (string, *string) {
	panic("unported: mj-core/src/acp.rs plan_review_answer")
}

// agentTaggedJSON encodes body as a JSON object and puts tagKey: tag in
// front of its members, as serde does for an internally tagged enum. A nil
// body encodes the tag alone. HTML characters are not escaped, as in serde.
func agentTaggedJSON(tagKey, tag string, body any) ([]byte, error) {
	head, err := agentMarshalJSON(map[string]string{tagKey: tag})
	if err != nil || body == nil {
		return head, err
	}
	members, err := agentMarshalJSON(body)
	if err != nil {
		return nil, err
	}
	if len(members) < 2 || members[0] != '{' {
		return nil, fmt.Errorf("internally tagged variant %s encodes as %s, not an object", tag, members)
	}
	if string(members) == "{}" {
		return head, nil
	}
	return append(append(head[:len(head)-1], ','), members[1:]...), nil
}

// agentExternalJSON encodes body as {"name": body}, serde's externally tagged
// form of a struct or newtype variant.
func agentExternalJSON(name string, body any) ([]byte, error) {
	return agentMarshalJSON(map[string]any{name: body})
}

// agentMarshalJSON encodes v without escaping HTML characters, which serde
// never escapes.
func agentMarshalJSON(v any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// agentTagOf reads the string member tagKey of a JSON object.
func agentTagOf(data []byte, tagKey string) (string, error) {
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return "", err
	}
	raw, ok := members[tagKey]
	if !ok {
		return "", fmt.Errorf("missing field `%s`", tagKey)
	}
	var tag string
	if err := json.Unmarshal(raw, &tag); err != nil {
		return "", err
	}
	return tag, nil
}

// agentExternalVariant splits serde's externally tagged form: a bare string
// for a unit variant, or a one-member object {"Name": body}.
func agentExternalVariant(data []byte) (string, json.RawMessage, error) {
	var name string
	if err := json.Unmarshal(data, &name); err == nil {
		return name, nil, nil
	}
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return "", nil, err
	}
	if len(members) != 1 {
		return "", nil, errors.New("expected an externally tagged enum with exactly one variant key")
	}
	for name, body := range members {
		return name, body, nil
	}
	panic("unreachable")
}

// agentDecodeStrict decodes data into v and refuses unknown members, as
// serde's deny_unknown_fields does.
func agentDecodeStrict(data []byte, v any) error {
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	return decoder.Decode(v)
}

// agentDecodeInto decodes data as the variant T of the sealed interface I.
func agentDecodeInto[T any, I any](data []byte) (I, error) {
	var v T
	if err := json.Unmarshal(data, &v); err != nil {
		var zero I
		return zero, err
	}
	return any(v).(I), nil
}

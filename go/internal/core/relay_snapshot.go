// Port of mj-core/src/relay/snapshot.rs.
//
// The deterministic relay state machine: commands, events, observations, and
// the snapshot they fold into. ApplyRelayEvent is the single place that turns
// one more event into the next snapshot; everything else here is either a
// type that shape describes, or the byte-budget/truncation and digest
// machinery that keeps events and snapshots bounded and verifiable. Nothing
// in this module touches the filesystem.
//
// This file holds the command, event and snapshot types. The machinery that
// works on them is split by responsibility: relay_snapshot_budget.go holds the
// byte budgets and the truncation that keeps them, relay_snapshot_digest.go
// the event digest chain and its validation, and relay_snapshot_apply.go the
// fold of one event into the next snapshot.

package core

import (
	"bytes"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
	"reflect"
	"strings"

	acpschema "github.com/BrokkAi/acp-go/schema"
)

// relayTextBlock is a text content block, Rust's ContentBlock::from(String).
func relayTextBlock(text string) acpschema.ContentBlock {
	return acpschema.ContentBlock{Kind: acpschema.ContentBlockKindText, Text: &acpschema.TextContent{Text: text}}
}

func processLines(running []string) string {
	lines := make([]string, len(running))
	for i, line := range running {
		lines[i] = "- " + line
	}
	return strings.Join(lines, "\n")
}

// holdMinutes is the hold's length for prompt text: "30 minutes", "1 minute".
func holdMinutes() string {
	minutes := HoldGiveUpMs / 60_000
	if minutes == 1 {
		return fmt.Sprintf("%d minute", minutes)
	}
	return fmt.Sprintf("%d minutes", minutes)
}

// HandbackReminderText is the reminder prompt: fixed when nothing is running,
// otherwise it names the processes and tells the child it may end its turn to
// wait for them.
func HandbackReminderText(running []string) string {
	if len(running) == 0 {
		return SubagentHandbackReminderText
	}
	return fmt.Sprintf(
		"[handback reminder] Your turn ended without delivering a report, and commands you started are still running:\n%s\nIf your assigned task is finished, call the mj-agents handback tool now with your full report, then stop. If you are waiting for these commands to finish, you may end your turn: Mjolnir will prompt you when they have all exited, or after %s. Otherwise continue working, and hand back when the task is done.",
		processLines(running),
		holdMinutes(),
	)
}

// HoldFollowUpText is the prompt Mjolnir sends a held child when its
// processes exit or the wait reaches HoldGiveUpMs.
func HoldFollowUpText(kind HoldFollowUpKind, running []string) string {
	if kind == HoldFollowUpKindProcessesExited {
		return "[background work finished] The commands you were waiting for have exited. Check their results, continue your assigned task, and call the mj-agents handback tool when it is done."
	}
	return fmt.Sprintf(
		"[background wait ended] You have waited %s and these commands are still running:\n%s\nCall the mj-agents handback tool now with what you have: what finished, what is still running, and what your parent needs to decide. Your parent can tell you to keep waiting; Mjolnir leaves these commands running until your parent's next turn ends.",
		holdMinutes(),
		processLines(running),
	)
}

// RelayCommandPromptBlocksForCommand is the effective prompt, including the
// worker-authored restart note. False when the command carries no prompt.
func RelayCommandPromptBlocksForCommand(command RelayCommand, commandID string) ([]acpschema.ContentBlock, bool) {
	panic("unported: mj-core/src/relay/snapshot.rs RelayCommand::prompt_blocks_for_command")
}

// RelayCommandPromptBlocks is the prompt a command sends, if it sends one.
func RelayCommandPromptBlocks(command RelayCommand) ([]acpschema.ContentBlock, bool) {
	panic("unported: mj-core/src/relay/snapshot.rs RelayCommand::prompt_blocks")
}

func RelayCommandMinimumProtocol(command RelayCommand) uint32 {
	panic("unported: mj-core/src/relay/snapshot.rs RelayCommand::minimum_protocol")
}

// RelayCommandIsQueueEntry reports whether this command waits its turn in the
// durable command queue.
func RelayCommandIsQueueEntry(command RelayCommand) bool {
	switch command.(type) {
	case RelayCommandPrompt, RelayCommandHandbackReminder, RelayCommandHoldFollowUp, RelayCommandMailboxWake,
		RelayCommandContinueAuthorizedWork, RelayCommandResumeAfterQuota, RelayCommandSetConfig:
		return true
	}
	return false
}

func RelayCommandIsRelayLocal(command RelayCommand) bool {
	switch command.(type) {
	case RelayCommandResolveSteering, RelayCommandSetSubagentWait, RelayCommandRemoveQueuedPrompt,
		RelayCommandClearQueuedPrompts, RelayCommandCompleteCheckpoint, RelayCommandReleaseCheckpoint,
		RelayCommandAdvanceRecoveryFloor, RelayCommandRecordNotice, RelayCommandSetQuotaRecovery,
		RelayCommandSeedAssessmentContext, RelayCommandDismissQuestion, RelayCommandClearNotifications,
		RelayCommandNotificationsRead, RelayCommandInstallPromptContext, RelayCommandDeliverMailboxEvent:
		return true
	}
	return false
}

func RelayCommandIsEffectfulACP(command RelayCommand) bool {
	switch command.(type) {
	case RelayCommandClearContext, RelayCommandPrompt, RelayCommandHandbackReminder, RelayCommandHoldFollowUp,
		RelayCommandMailboxWake, RelayCommandContinueAuthorizedWork, RelayCommandResumeAfterQuota,
		RelayCommandSetConfig, RelayCommandRestoreExecutionMode, RelayCommandGoalControl,
		RelayCommandSetSessionMode, RelayCommandCancelTurn, RelayCommandSteer, RelayCommandCancelTurnFor,
		RelayCommandApplyQueuedFor, RelayCommandCancel, RelayCommandClose:
		return true
	}
	return false
}

func RelayCommandIsEffectfulUserShell(command RelayCommand) bool {
	switch command.(type) {
	case RelayCommandRunUserShell, RelayCommandCancelUserShell:
		return true
	}
	return false
}

// RelayCommandKindOf is Rust's RelayCommand::kind.
func RelayCommandKindOf(command RelayCommand) RelayCommandKind {
	panic("unported: mj-core/src/relay/snapshot.rs RelayCommand::kind")
}

type RelayCommandKind string

const (
	RelayCommandKindSteer                RelayCommandKind = "steer"
	RelayCommandKindResolveSteering      RelayCommandKind = "resolve_steering"
	RelayCommandKindClearContext         RelayCommandKind = "clear_context"
	RelayCommandKindPrompt               RelayCommandKind = "prompt"
	RelayCommandKindRunUserShell         RelayCommandKind = "run_user_shell"
	RelayCommandKindCancelUserShell      RelayCommandKind = "cancel_user_shell"
	RelayCommandKindRemoveQueuedPrompt   RelayCommandKind = "remove_queued_prompt"
	RelayCommandKindClearQueuedPrompts   RelayCommandKind = "clear_queued_prompts"
	RelayCommandKindSetConfig            RelayCommandKind = "set_config"
	RelayCommandKindRestoreExecutionMode RelayCommandKind = "restore_execution_mode"
	RelayCommandKindGoalControl          RelayCommandKind = "goal_control"
	RelayCommandKindSetSessionMode       RelayCommandKind = "set_session_mode"
	RelayCommandKindCancelTurn           RelayCommandKind = "cancel_turn"
	RelayCommandKindCancel               RelayCommandKind = "cancel"
	RelayCommandKindClose                RelayCommandKind = "close"
	RelayCommandKindBeginCheckpoint      RelayCommandKind = "begin_checkpoint"
	RelayCommandKindCompleteCheckpoint   RelayCommandKind = "complete_checkpoint"
	RelayCommandKindReleaseCheckpoint    RelayCommandKind = "release_checkpoint"
	RelayCommandKindAdvanceRecoveryFloor RelayCommandKind = "advance_recovery_floor"
	RelayCommandKindRecordNotice         RelayCommandKind = "record_notice"
)

func (k RelayCommandKind) AsStr() string { return string(k) }

// SteeringStatus is durable delivery state, independent of transport acceptance.
type SteeringStatus string

const (
	SteeringStatusPending     SteeringStatus = "pending"
	SteeringStatusUnconfirmed SteeringStatus = "unconfirmed"
	SteeringStatusFailed      SteeringStatus = "failed"
	SteeringStatusApplied     SteeringStatus = "applied"
	SteeringStatusResolved    SteeringStatus = "resolved"
)

type SteeringOperation struct {
	CommandID      string         `json:"command_id"`
	ActivePromptID string         `json:"active_prompt_id"`
	QueuedPromptID string         `json:"queued_prompt_id"`
	Status         SteeringStatus `json:"status"`
	Message        *string        `json:"message"`
}

func (o SteeringOperation) HoldsQueue() bool {
	return o.Status == SteeringStatusPending || o.Status == SteeringStatusUnconfirmed
}

// QueuedRelayPrompt is the payload-free queue identity exposed in
// attach/status responses.
type QueuedRelayPrompt struct {
	CommandID   string `json:"command_id"`
	CreatedAtMs int64  `json:"created_at_ms"`
}

// ActiveRelayPrompt is the payload-free active prompt identity exposed in
// attach/status responses.
type ActiveRelayPrompt struct {
	CommandID   string `json:"command_id"`
	CreatedAtMs int64  `json:"created_at_ms"`
	StartedAtMs int64  `json:"started_at_ms"`
}

type ActiveUserShell struct {
	CommandID   string `json:"command_id"`
	Command     string `json:"command"`
	CreatedAtMs int64  `json:"created_at_ms"`
	StartedAtMs *int64 `json:"started_at_ms,omitempty"`
}

// ActiveAgentTerminal is a terminal the ACP agent asked Hel to run on its
// behalf.
//
// Unlike a transcript tool call, this is live operational state: it exists
// only while the child process is alive and lets clients show truthful
// activity when an agent fails to publish the matching ACP tool update.
type ActiveAgentTerminal struct {
	TerminalID  string `json:"terminal_id"`
	Command     string `json:"command"`
	StartedAtMs int64  `json:"started_at_ms"`
}

// BackgroundCommand is a command the agent left running with nothing waiting
// on it.
//
// Harnesses produce these differently: Hel-hosted terminals that outlive
// their tool card, provider-owned task levels, and Codex exec cards whose
// result carries no exit code. The relay reduces them to this one shape so
// every surface renders them the same way.
type BackgroundCommand struct {
	// Process-local identity. Clients must treat this as opaque and return it
	// unchanged when requesting a stop.
	ID          string `json:"id"`
	StartedAtMs int64  `json:"started_at_ms"`
	Command     string `json:"command"`
	// Whether the current worker can stop this task without ending its turn.
	CanStop bool `json:"can_stop"`
}

// BackgroundTaskStopTarget is the process owner behind one stoppable
// background-command id.
//
// This never crosses the relay wire. The worker resolves the opaque public id
// against its current live state before handing the target to the ACP bridge.
type BackgroundTaskStopTarget interface {
	isBackgroundTaskStopTarget()
}

type BackgroundTaskStopTargetNativeAgent struct {
	SessionID string
}

type BackgroundTaskStopTargetHostedTerminal struct {
	TerminalID string
}

type BackgroundTaskStopTargetAirAsyncTask struct {
	SessionID string
	TaskID    string
}

func (BackgroundTaskStopTargetNativeAgent) isBackgroundTaskStopTarget()    {}
func (BackgroundTaskStopTargetHostedTerminal) isBackgroundTaskStopTarget() {}
func (BackgroundTaskStopTargetAirAsyncTask) isBackgroundTaskStopTarget()   {}

// HarnessTurn is a turn the harness started on its own, with no prompt in
// flight.
//
// Claude Code re-invokes itself when a background task it started finishes.
// The adapter streams that work through ordinary session/update notifications
// and settles it with a usage_update carrying an origin marker, so the relay
// models it as a turn rather than as idle chatter.
type HarnessTurn struct {
	StartedAtMs int64 `json:"started_at_ms"`
}

type UserShellStatus string

const (
	UserShellStatusExited      UserShellStatus = "exited"
	UserShellStatusSignaled    UserShellStatus = "signaled"
	UserShellStatusTimedOut    UserShellStatus = "timed_out"
	UserShellStatusCancelled   UserShellStatus = "cancelled"
	UserShellStatusInterrupted UserShellStatus = "interrupted"
	UserShellStatusFailed      UserShellStatus = "failed"
)

// UserShellResult refuses unknown fields.
type UserShellResult struct {
	Command         string          `json:"command"`
	Stdout          string          `json:"stdout"`
	Stderr          string          `json:"stderr"`
	StdoutTruncated bool            `json:"stdout_truncated"`
	StderrTruncated bool            `json:"stderr_truncated"`
	ExitCode        *int32          `json:"exit_code,omitempty"`
	Signal          *string         `json:"signal,omitempty"`
	DurationMs      uint64          `json:"duration_ms"`
	Status          UserShellStatus `json:"status"`
	Error           *string         `json:"error,omitempty"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (r *UserShellResult) UnmarshalJSON(data []byte) error {
	type plain UserShellResult
	if err := relayRejectUnknownFields(data, reflect.TypeFor[plain](), ""); err != nil {
		return err
	}
	return json.Unmarshal(data, (*plain)(r))
}

func (r UserShellResult) PromptContext() string {
	panic("unported: mj-core/src/relay/snapshot.rs UserShellResult::prompt_context")
}

// StoredQueuedRelayCommand is one entry of the durable command queue. Prompts
// and configuration changes share the queue so they run in the order the
// user submitted them.
//
// The payload is untagged and flattened so entries written before
// configuration changes could be queued still load: they carry a prompt
// field and nothing else.
type StoredQueuedRelayCommand struct {
	CommandID   string
	Payload     StoredQueuedRelayPayload
	CreatedAtMs int64
}

// MarshalJSON writes command_id, the flattened payload fields, then
// created_at_ms.
func (c StoredQueuedRelayCommand) MarshalJSON() ([]byte, error) {
	switch payload := c.Payload.(type) {
	case StoredQueuedRelayPayloadPrompt:
		return relayMarshalJSON(struct {
			CommandID   string                   `json:"command_id"`
			Prompt      []acpschema.ContentBlock `json:"prompt"`
			CreatedAtMs int64                    `json:"created_at_ms"`
		}{c.CommandID, payload.Prompt, c.CreatedAtMs})
	case StoredQueuedRelayPayloadSetConfig:
		return relayMarshalJSON(struct {
			CommandID   string `json:"command_id"`
			Key         string `json:"key"`
			Value       string `json:"value"`
			CreatedAtMs int64  `json:"created_at_ms"`
		}{c.CommandID, payload.Key, payload.Value, c.CreatedAtMs})
	}
	return nil, fmt.Errorf("stored queued relay command %s has no payload", c.CommandID)
}

// UnmarshalJSON reads the flattened payload as the first untagged variant
// whose fields are present: a prompt, then a configuration change.
func (c *StoredQueuedRelayCommand) UnmarshalJSON(data []byte) error {
	var wire struct {
		CommandID   *string                   `json:"command_id"`
		Prompt      *[]acpschema.ContentBlock `json:"prompt"`
		Key         *string                   `json:"key"`
		Value       *string                   `json:"value"`
		CreatedAtMs *int64                    `json:"created_at_ms"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	if wire.CommandID == nil {
		return errors.New("missing field `command_id`")
	}
	if wire.CreatedAtMs == nil {
		return errors.New("missing field `created_at_ms`")
	}
	switch {
	case wire.Prompt != nil:
		c.Payload = StoredQueuedRelayPayloadPrompt{Prompt: *wire.Prompt}
	case wire.Key != nil && wire.Value != nil:
		c.Payload = StoredQueuedRelayPayloadSetConfig{Key: *wire.Key, Value: *wire.Value}
	default:
		return errors.New("data did not match any variant of untagged enum StoredQueuedRelayPayload")
	}
	c.CommandID, c.CreatedAtMs = *wire.CommandID, *wire.CreatedAtMs
	return nil
}

// StoredQueuedRelayPayload is the untagged payload of a queue entry; it is
// only ever serialized flattened into StoredQueuedRelayCommand.
type StoredQueuedRelayPayload interface {
	isStoredQueuedRelayPayload()
}

type StoredQueuedRelayPayloadPrompt struct {
	Prompt []acpschema.ContentBlock `json:"prompt"`
}

type StoredQueuedRelayPayloadSetConfig struct {
	Key   string `json:"key"`
	Value string `json:"value"`
}

func (StoredQueuedRelayPayloadPrompt) isStoredQueuedRelayPayload()    {}
func (StoredQueuedRelayPayloadSetConfig) isStoredQueuedRelayPayload() {}

type StoredActiveRelayPrompt struct {
	CommandID   string                   `json:"command_id"`
	Prompt      []acpschema.ContentBlock `json:"prompt"`
	CreatedAtMs int64                    `json:"created_at_ms"`
	StartedAtMs int64                    `json:"started_at_ms"`
}

type RelayExecutionState string

const (
	RelayExecutionStateIdle    RelayExecutionState = "idle"
	RelayExecutionStateRunning RelayExecutionState = "running"
	RelayExecutionStateClosing RelayExecutionState = "closing"
	RelayExecutionStateClosed  RelayExecutionState = "closed"
)

// RelayCursor refuses unknown fields.
type RelayCursor struct {
	Ordinal uint64 `json:"ordinal"`
	Digest  string `json:"digest"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (c *RelayCursor) UnmarshalJSON(data []byte) error {
	type plain RelayCursor
	if err := relayRejectUnknownFields(data, reflect.TypeFor[plain](), ""); err != nil {
		return err
	}
	return json.Unmarshal(data, (*plain)(c))
}

// RelayOperationalState is the public operational state a worker publishes.
//
// Rust held several fields behind Arc to share them between snapshots; here
// they are plain values. Slices without omitempty must be non-nil to encode
// as [] rather than null.
type RelayOperationalState struct {
	Assessment  *TurnAssessment `json:"assessment,omitempty"`
	OpenRequest *OpenRequest    `json:"open_request,omitempty"`
	// Always serialized (Rust Arc<NotificationStack> with serde default).
	NotificationStack        NotificationStack `json:"notification_stack"`
	NotificationsReadThrough uint64            `json:"notifications_read_through"`
	// Retained for checkpoint export; attention reads OpenRequest.
	PersonInputThrough uint64 `json:"person_input_through"`
	// Whether the latest finished turn's work processes are still alive.
	WorkLiveness *WorkLiveness `json:"work_liveness,omitempty"`
	// Keys of mailbox events the worker accepted but has not yet handed to the
	// agent: pending ones and those leased to a hook. The full delivered-key
	// map stays in the snapshot; this list is short.
	UndeliveredMailboxEventKeys []string        `json:"undelivered_mailbox_event_keys,omitempty"`
	AssessmentContext           *ContextHistory `json:"assessment_context,omitempty"`
	// The latest prompt's completion identity; see TurnCompletion.
	TurnCompletion *TurnCompletion   `json:"turn_completion,omitempty"`
	Continuation   ContinuationState `json:"continuation"`
	// Negotiated connection protocol, supplied by the controller after hello.
	RelayProtocolVersion    *uint32            `json:"relay_protocol_version,omitempty"`
	NativeAgents            []NativeAgent      `json:"native_agents,omitempty"`
	Steering                *SteeringOperation `json:"steering,omitempty"`
	CancellingPromptID      *string            `json:"cancelling_prompt_id,omitempty"`
	ClearContext            bool               `json:"clear_context"`
	ClearContextStartedAtMs *int64             `json:"clear_context_started_at_ms,omitempty"`
	NativeAgentCount        int                `json:"native_agent_count"`
	// Process-local inference; a restarted worker must forget it.
	ExpectedContinuation *int64 `json:"expected_continuation,omitempty"`
	// Process-local Jev inference, never recovered as proof of idle.
	InferredIdleSinceMs *int64 `json:"inferred_idle_since_ms,omitempty"`
	// When a background task the harness will follow up on settled, while no
	// turn has opened since. See ActivityFacts.TaskSettledAtMs.
	TaskSettledAtMs *int64 `json:"task_settled_at_ms,omitempty"`
	// Process-local Jev judgment of the leftover background processes.
	// See ActivityFacts.BackgroundNeeded.
	BackgroundNeeded       *bool          `json:"background_needed,omitempty"`
	Goal                   GoalState      `json:"goal"`
	CapacityRetry          *CapacityRetry `json:"capacity_retry,omitempty"`
	RetryAssessmentPending bool           `json:"retry_assessment_pending"`
	// Retired at protocol 70: workers no longer set it, and no reader asks
	// it. It stays so that an older daemon still decodes the snapshot.
	AuthenticationRetryWaiting *string `json:"authentication_retry_waiting,omitempty"`
	// Worker-published: the access-token hash of the failed turn this
	// worker still owes a retry for. Present while the durable intent holds
	// (the turn failed authentication, nothing newer was accepted or
	// started, nobody withdrew or paused the work), even while a checkpoint
	// or other transient state delays the submit. Absent from older workers.
	AuthenticationRetryIntent *string `json:"authentication_retry_intent,omitempty"`
	// Identity observed in the session's credential store. Claude reports
	// its access-token hash; other harnesses report the credential file hash.
	HeldLoginFingerprint *string `json:"held_login_fingerprint,omitempty"`
	SessionID            string  `json:"session_id"`
	// Start of the latest turn, retained until its background work settles.
	ActivityTurnStartedAtMs *int64 `json:"activity_turn_started_at_ms,omitempty"`
	// Durable identity of this relay store, replaced by a fresh restore.
	StoreID *string `json:"store_id,omitempty"`
	// Start of the current observed idle period; older workers leave it unknown.
	IdleSinceMs         *int64              `json:"idle_since_ms,omitempty"`
	Execution           RelayExecutionState `json:"execution"`
	LatestOrdinal       uint64              `json:"latest_ordinal"`
	LatestDigest        string              `json:"latest_digest"`
	AcknowledgedThrough uint64              `json:"acknowledged_through"`
	AcknowledgedDigest  string              `json:"acknowledged_digest"`
	// Highest verified checkpoint frontier. Events newer than this remain in
	// the relay journal even after acknowledgement.
	RecoveryFloorOrdinal uint64  `json:"recovery_floor_ordinal"`
	RecoveryFloorDigest  string  `json:"recovery_floor_digest"`
	NativeSessionID      *string `json:"native_session_id"`
	// Whether this native session replaced one that could not be reloaded.
	// Only harnesses whose checkpoints carry no native session files fall
	// back this way, so the transcript is the only surviving context. It
	// stays set for the life of the native session and clears when a normal
	// open records a new native id.
	NativeContinuityLost bool `json:"native_continuity_lost"`
	// The native session the current one replaced because it was never used
	// (RelayObservationSessionOpened.ReplacedUnusedNativeSessionID).
	ReplacedUnusedNativeSessionID *string `json:"replaced_unused_native_session_id,omitempty"`
	// Whether the current worker process has finished opening its ACP
	// session. Older workers omit this field and are treated as ready.
	ACPReady *bool `json:"acp_ready,omitempty"`
	// Where this worker process is in preparing its harness. Older workers
	// omit it; they accept connections only after preparation finishes.
	HarnessPreparation HarnessPreparation `json:"harness_preparation,omitempty"`
	// This process serves recovered state without running a harness.
	CheckpointOnly    bool                         `json:"checkpoint_only"`
	AgentCapabilities *acpschema.AgentCapabilities `json:"agent_capabilities"`
	AgentInfo         *acpschema.Implementation    `json:"agent_info"`
	Runtime           *RuntimeReceipt              `json:"runtime,omitempty"`
	// Older workers do not report the optional ACP steering extension.
	SteeringSupported *bool `json:"steering_supported,omitempty"`
	// When a message to this working session is delivered, as the worker
	// enforces it. Absent from older workers and from harnesses whose
	// delivery the worker has not classified; absence never means tool
	// boundary.
	MailboxDelivery *MailboxDeliveryMode `json:"mailbox_delivery,omitempty"`
	// Actions decided by the worker against its ordered command queue.
	TurnControls         *TurnControls                   `json:"turn_controls,omitempty"`
	ConfigOptions        []acpschema.SessionConfigOption `json:"config_options"`
	Modes                *acpschema.SessionModeState     `json:"modes,omitempty"`
	AvailableCommands    []acpschema.AvailableCommand    `json:"available_commands"`
	Config               map[string]string               `json:"config"`
	ActivePrompt         *ActiveRelayPrompt              `json:"active_prompt"`
	QueuedPrompts        []QueuedRelayPrompt             `json:"queued_prompts"`
	ActiveUserShells     []ActiveUserShell               `json:"active_user_shells,omitempty"`
	ActiveAgentTerminals []ActiveAgentTerminal           `json:"active_agent_terminals,omitempty"`
	CheckpointBarrier    *string                         `json:"checkpoint_barrier"`
	CheckpointReady      *RelayCursor                    `json:"checkpoint_ready"`
	// When anything at all last arrived over ACP. This is the liveness
	// signal — a bridge that has gone quiet — not the step clock: a
	// streaming message refreshes it many times a second.
	LastACPActivityAtMs *int64 `json:"last_acp_activity_at_ms,omitempty"`
	// When the step the agent is on began, in epoch milliseconds, while a
	// step is in flight. A worker too old to report it leaves this empty and
	// the step clock falls back to the turn it belongs to.
	CurrentStepStartedAtMs *int64 `json:"current_step_started_at_ms,omitempty"`
	// When the newest tool call still reporting pending or in-progress
	// started its current status. Unlike the general step clock, this is
	// positive evidence of foreground work even when the harness exposes no
	// turn boundary of its own.
	//
	// Kept for a daemon too old to read ToolsInFlight, and derived from
	// it; new code reads ToolsInFlight, which also carries the age of the
	// oldest call, which is what bounds a turn.
	ForegroundToolStartedAtMs *int64 `json:"foreground_tool_started_at_ms,omitempty"`
	// Every tool call the agent has open, oldest first. A harness that marks
	// no turn of its own is visible only this way, and a turn blocked in a
	// long tool call is proved to be working by it.
	ToolsInFlight []InFlightToolCall `json:"tools_in_flight,omitempty"`
	// The worker's own answer to what this session is doing.
	//
	// Published so no consumer re-derives one. A daemon reading an older
	// worker that does not send it classifies Facts with the same
	// function the worker used, so the answer is identical either way;
	// there is no second implementation.
	// Nil when absent.
	Activity ActivityState `json:"activity,omitempty"`
	// The turn the harness started on its own, while it is open.
	HarnessTurn *HarnessTurn `json:"harness_turn,omitempty"`
	// Ordinal of the newest harness_turn_started event, whether or not that
	// turn is still open. It only moves forward, so a checkpoint can compare
	// it against the cursor it captured.
	LastHarnessTurnStartedOrdinal *uint64 `json:"last_harness_turn_started_ordinal,omitempty"`
	// Commands the agent left running while nothing waits on them, oldest
	// first. Live operational state, like ActiveAgentTerminals: it is
	// derived from what the relay can see now, not from the journal.
	BackgroundCommands []BackgroundCommand `json:"background_commands,omitempty"`
	// Whether provider-owned background work is known for this worker
	// process. Only Kimi needs this today; older workers and other harnesses
	// omit it.
	BackgroundWorkKnown *bool `json:"background_work_known,omitempty"`
}

// UnmarshalJSON decodes HarnessPreparation through its variant table.
func (s *RelayOperationalState) UnmarshalJSON(data []byte) error {
	type plain RelayOperationalState
	aux := struct {
		*plain
		HarnessPreparation json.RawMessage `json:"harness_preparation"`
	}{plain: (*plain)(s)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	s.HarnessPreparation = nil
	if len(aux.HarnessPreparation) > 0 && string(aux.HarnessPreparation) != "null" {
		preparation, err := UnmarshalHarnessPreparationJSON(aux.HarnessPreparation)
		if err != nil {
			return err
		}
		s.HarnessPreparation = preparation
	}
	return nil
}

// RetainedEventOrdinal interprets the worker-owned journal boundary using its
// negotiated policy. Older workers collected only through both ACK and
// recovery floor.
func (s *RelayOperationalState) RetainedEventOrdinal() uint64 {
	protocol := RelayProtocolVersion
	if s.RelayProtocolVersion != nil {
		protocol = *s.RelayProtocolVersion
	}
	return retainedEventOrdinal(protocol, s.AcknowledgedThrough, s.RecoveryFloorOrdinal)
}

func (s *RelayOperationalState) SupportsNotificationStack() bool {
	return s.RelayProtocolVersion != nil && *s.RelayProtocolVersion >= RelayNotificationStackProtocol
}

// SupportsOpenRequest reports whether this worker owns a persistent open
// request.
func (s *RelayOperationalState) SupportsOpenRequest() bool {
	return s.RelayProtocolVersion != nil && *s.RelayProtocolVersion >= RelayOpenRequestProtocol
}

// QuestionMessage is the original question target, with compatibility for
// pre-64 workers.
func (s *RelayOperationalState) QuestionMessage() (string, bool) {
	panic("unported: mj-core/src/relay/snapshot.rs RelayOperationalState::question_message")
}

// ShareUnchangedFields reuses unchanged payloads after decoding a new worker
// response.
func (s *RelayOperationalState) ShareUnchangedFields(previous *RelayOperationalState) {
	panic("unported: mj-core/src/relay/snapshot.rs RelayOperationalState::share_unchanged_fields")
}

// AcceptsPromptImages: missing initialization advertises no support for
// image prompts.
func (s *RelayOperationalState) AcceptsPromptImages() bool {
	return s.AgentCapabilities != nil && s.AgentCapabilities.PromptCapabilities != nil &&
		s.AgentCapabilities.PromptCapabilities.Image != nil && *s.AgentCapabilities.PromptCapabilities.Image
}

// SupportsTargetedTurnControl: targeted turn control was introduced with
// relay protocol 17.
func (s *RelayOperationalState) SupportsTargetedTurnControl() bool {
	return s.RelayProtocolVersion != nil &&
		*s.RelayProtocolVersion >= RelayCommandMinimumProtocol(RelayCommandCancelTurnFor{})
}

// NativeSessionIsReady reports whether this worker has a usable native ACP
// session.
//
// Workers predating acp_ready are treated as ready for compatibility; a
// current worker that explicitly reports not ready is authoritative.
func (s *RelayOperationalState) NativeSessionIsReady() bool {
	return !s.CheckpointOnly && s.Execution != RelayExecutionStateClosed && s.NativeSessionID != nil &&
		(s.ACPReady == nil || *s.ACPReady)
}

// Facts is everything that bears on whether this session is working, in the
// shape the activity module decides with.
//
// This is the only translation from a relay snapshot into an activity
// decision. Every predicate below goes through it, so none of them can read a
// different subset of the facts than another.
func (s *RelayOperationalState) Facts() ActivityFacts {
	panic("unported: mj-core/src/relay/snapshot.rs RelayOperationalState::facts")
}

// ActivityState is what this session is doing: the worker's published
// answer, or the same classification of the same facts when the worker is
// too old to publish one.
func (s *RelayOperationalState) ActivityState() ActivityState {
	panic("unported: mj-core/src/relay/snapshot.rs RelayOperationalState::activity_state")
}

// IsQuiet reports whether nothing the worker owns would be destroyed by
// killing it now.
//
// Stopping a worker tears down the ACP bridge with it, so any operation that
// replaces a worker in place has to wait for this. A held checkpoint barrier
// is itself a reason to wait; HasWorkInFlight is the same predicate without
// it, for a caller that holds the barrier and is asking whether anything else
// is running.
func (s *RelayOperationalState) IsQuiet() bool {
	panic("unported: mj-core/src/relay/snapshot.rs RelayOperationalState::is_quiet")
}

// HasWorkInFlight reports whether the session still owns foreground or
// background work.
func (s *RelayOperationalState) HasWorkInFlight() bool {
	panic("unported: mj-core/src/relay/snapshot.rs RelayOperationalState::has_work_in_flight")
}

// SafeToReplace reports whether a controller may replace this worker without
// losing work.
func (s *RelayOperationalState) SafeToReplace(harness HarnessKind) bool {
	panic("unported: mj-core/src/relay/snapshot.rs RelayOperationalState::safe_to_replace")
}

// SafeToMove is Move's purpose-specific interruption decision.
func (s *RelayOperationalState) SafeToMove(harness HarnessKind, purpose MovePurpose) bool {
	panic("unported: mj-core/src/relay/snapshot.rs RelayOperationalState::safe_to_move")
}

// HarnessPreparationFailed: terminal startup failure; this worker cannot
// retry preparation in place.
func (s *RelayOperationalState) HarnessPreparationFailed() bool {
	_, failed := s.HarnessPreparation.(HarnessPreparationFailed)
	return failed
}

// Quiet is why the session is or is not quiet right now, for a log line.
func (s *RelayOperationalState) Quiet() Quiet {
	panic("unported: mj-core/src/relay/snapshot.rs RelayOperationalState::quiet")
}

// SafeForCheckpoint reports whether a routine checkpoint may admit a barrier
// without risking provider-owned goal or background work. Unknown native
// state fails closed.
func (s *RelayOperationalState) SafeForCheckpoint(harness HarnessKind) bool {
	_, blocked := s.CheckpointBackgroundBlocker(harness)
	return !blocked
}

// CheckpointBackgroundBlocker is the same provider-owned work prerequisite
// used by checkpoint admission.
func (s *RelayOperationalState) CheckpointBackgroundBlocker(harness HarnessKind) (string, bool) {
	panic("unported: mj-core/src/relay/snapshot.rs RelayOperationalState::checkpoint_background_blocker")
}

// RoutineCheckpointWait is whether a routine checkpoint may start now
// (nil), or why it has to wait. See the activity module's
// RoutineCheckpointWait.
func (s *RelayOperationalState) RoutineCheckpointWait(harness HarnessKind) *CheckpointWait {
	panic("unported: mj-core/src/relay/snapshot.rs RelayOperationalState::routine_checkpoint_wait")
}

// On-disk record format for a relay event.
//   - 1 (chained): folds previous_digest into the digest, forming a hash
//     chain. This is the legacy format; a record with no format key on disk is
//     read as v1.
//   - 2 (self-describing): carries no previous_digest; the digest depends
//     only on the record's own content, so a corrupt record cannot invalidate
//     its neighbours.
const (
	RelayEventFormatV1 uint8 = 1
	RelayEventFormatV2 uint8 = 2
)

func defaultRelayEventFormat() uint8 { return RelayEventFormatV1 }

func isRelayEventFormatV1(format uint8) bool { return format == RelayEventFormatV1 }

type RelayEvent struct {
	// Record format. Skipped on the wire when v1 so existing v1 journals
	// round-trip byte-for-byte and old records (no format key) read as v1.
	Format  uint8  `json:"format"`
	Ordinal uint64 `json:"ordinal"`
	// Predecessor digest, forming the v1 chain. Absent (empty) for v2 records.
	PreviousDigest string           `json:"previous_digest,omitempty"`
	Digest         string           `json:"digest"`
	RecordedAtMs   int64            `json:"recorded_at_ms"`
	CommandID      *string          `json:"command_id,omitempty"`
	Observation    RelayObservation `json:"observation"`
}

// MarshalJSON omits format for v1 records.
func (e RelayEvent) MarshalJSON() ([]byte, error) {
	var format *uint8
	if !isRelayEventFormatV1(e.Format) {
		format = &e.Format
	}
	return relayMarshalJSON(struct {
		Format         *uint8           `json:"format,omitempty"`
		Ordinal        uint64           `json:"ordinal"`
		PreviousDigest string           `json:"previous_digest,omitempty"`
		Digest         string           `json:"digest"`
		RecordedAtMs   int64            `json:"recorded_at_ms"`
		CommandID      *string          `json:"command_id,omitempty"`
		Observation    RelayObservation `json:"observation"`
	}{format, e.Ordinal, e.PreviousDigest, e.Digest, e.RecordedAtMs, e.CommandID, e.Observation})
}

// UnmarshalJSON reads a missing format as v1 and decodes the observation
// through its variant table.
func (e *RelayEvent) UnmarshalJSON(data []byte) error {
	type plain RelayEvent
	aux := struct {
		*plain
		Format      *uint8          `json:"format"`
		Observation json.RawMessage `json:"observation"`
	}{plain: (*plain)(e)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	e.Format = defaultRelayEventFormat()
	if aux.Format != nil {
		e.Format = *aux.Format
	}
	observation, err := UnmarshalRelayObservationJSON(aux.Observation)
	if err != nil {
		return err
	}
	e.Observation = observation
	return nil
}

type MailboxHookLeaseReturnReason string

const (
	MailboxHookLeaseReturnReasonTimeout       MailboxHookLeaseReturnReason = "timeout"
	MailboxHookLeaseReturnReasonWorkerRestart MailboxHookLeaseReturnReason = "worker_restart"
	MailboxHookLeaseReturnReasonTurnEnded     MailboxHookLeaseReturnReason = "turn_ended"
)

// MailboxHookLease refuses unknown fields.
type MailboxHookLease struct {
	LeaseID     string         `json:"lease_id"`
	Events      []MailboxEvent `json:"events"`
	HookEvent   string         `json:"hook_event"`
	ExpiresAtMs int64          `json:"expires_at_ms"`
	// A queued user prompt delivered as context at this tool boundary.
	// The queue remains the authoritative source for the prompt text.
	Steering *MailboxHookSteering `json:"steering,omitempty"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (l *MailboxHookLease) UnmarshalJSON(data []byte) error {
	type plain MailboxHookLease
	if err := relayRejectUnknownFields(data, reflect.TypeFor[plain](), ""); err != nil {
		return err
	}
	return json.Unmarshal(data, (*plain)(l))
}

// MailboxHookSteering refuses unknown fields.
type MailboxHookSteering struct {
	CommandID      string `json:"command_id"`
	ActivePromptID string `json:"active_prompt_id"`
	QueuedPromptID string `json:"queued_prompt_id"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (s *MailboxHookSteering) UnmarshalJSON(data []byte) error {
	type plain MailboxHookSteering
	if err := relayRejectUnknownFields(data, reflect.TypeFor[plain](), ""); err != nil {
		return err
	}
	return json.Unmarshal(data, (*plain)(s))
}

// serializeInitializedCapabilities: protocol 71 peers hash the
// pre-systemPrompt AgentInitialized wire shape. Negotiation uses the original
// capabilities; only this journal view omits it.
func serializeInitializedCapabilities(capabilities acpschema.AgentCapabilities) ([]byte, error) {
	compatible := capabilities
	if capabilities.SessionCapabilities != nil {
		session := *capabilities.SessionCapabilities
		session.SystemPrompt = nil
		compatible.SessionCapabilities = &session
	}
	return relayMarshalJSON(compatible)
}

// MarshalJSON writes the capabilities without session_capabilities.system_prompt.
func (v RelayObservationAgentInitialized) MarshalJSON() ([]byte, error) {
	capabilities, err := serializeInitializedCapabilities(v.Capabilities)
	if err != nil {
		return nil, err
	}
	return relayAdjacentJSON("type", "data", "agent_initialized", struct {
		ProtocolVersion acpschema.ProtocolVersion `json:"protocol_version"`
		Capabilities    json.RawMessage           `json:"capabilities"`
		AgentInfo       *acpschema.Implementation `json:"agent_info"`
		Runtime         *RuntimeIdentity          `json:"runtime,omitempty"`
	}{v.ProtocolVersion, capabilities, v.AgentInfo, v.Runtime})
}

type ClaimedSteeringPrompt struct {
	// Runtime-only store location; never sent or persisted.
	AttachmentRoot  *string                  `json:"-"`
	QueuedCommandID string                   `json:"queued_command_id"`
	Prompt          []acpschema.ContentBlock `json:"prompt"`
}

type ClaimedRelayCommand struct {
	CommandID           string                 `json:"command_id"`
	AcceptedOrdinal     uint64                 `json:"accepted_ordinal"`
	Command             RelayCommand           `json:"command"`
	HiddenPromptContext *string                `json:"hidden_prompt_context,omitempty"`
	SteeringPrompt      *ClaimedSteeringPrompt `json:"steering_prompt,omitempty"`
}

// UnmarshalJSON decodes Command through its variant table.
func (c *ClaimedRelayCommand) UnmarshalJSON(data []byte) error {
	type plain ClaimedRelayCommand
	aux := struct {
		*plain
		Command json.RawMessage `json:"command"`
	}{plain: (*plain)(c)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	command, err := UnmarshalRelayCommandJSON(aux.Command)
	if err != nil {
		return err
	}
	c.Command = command
	return nil
}

type PendingPromptContext struct {
	Text              string  `json:"text"`
	AttachedCommandID *string `json:"attached_command_id,omitempty"`
}

type PendingUserShellContext struct {
	ShellCommandID    string  `json:"shell_command_id"`
	AcceptedOrdinal   uint64  `json:"accepted_ordinal"`
	Text              string  `json:"text"`
	AttachedCommandID *string `json:"attached_command_id,omitempty"`
}

type RelayDispatchState string

const (
	RelayDispatchStateQueued      RelayDispatchState = "queued"
	RelayDispatchStatePending     RelayDispatchState = "pending"
	RelayDispatchStateInFlight    RelayDispatchState = "in_flight"
	RelayDispatchStateCompleted   RelayDispatchState = "completed"
	RelayDispatchStateRejected    RelayDispatchState = "rejected"
	RelayDispatchStateInterrupted RelayDispatchState = "interrupted"
)

type RelayDispatchRecord struct {
	Command RelayCommand       `json:"command"`
	State   RelayDispatchState `json:"state"`
}

// UnmarshalJSON decodes Command through its variant table.
func (r *RelayDispatchRecord) UnmarshalJSON(data []byte) error {
	type plain RelayDispatchRecord
	aux := struct {
		*plain
		Command json.RawMessage `json:"command"`
	}{plain: (*plain)(r)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	command, err := UnmarshalRelayCommandJSON(aux.Command)
	if err != nil {
		return err
	}
	r.Command = command
	return nil
}

// StoredHarnessTurn is the durable half of an open harness-initiated turn.
// Unknown fields are refused.
type StoredHarnessTurn struct {
	// Whether no Mjolnir prompt was in flight when this turn began.
	Autonomous  bool  `json:"autonomous"`
	StartedAtMs int64 `json:"started_at_ms"`
	// Ordinal of the harness_turn_started event that opened this turn.
	FirstOrdinal uint64 `json:"first_ordinal"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (t *StoredHarnessTurn) UnmarshalJSON(data []byte) error {
	type plain StoredHarnessTurn
	if err := relayRejectUnknownFields(data, reflect.TypeFor[plain](), ""); err != nil {
		return err
	}
	return json.Unmarshal(data, (*plain)(t))
}

// HarnessTurnCancel is a stop aimed at one open autonomous harness turn,
// named by the ordinal that opened it. The journal's Cancel/CancelTurn events
// own it. Unknown fields are refused.
type HarnessTurnCancel struct {
	FirstOrdinal uint64 `json:"first_ordinal"`
	// The runtime acknowledged a stop of this turn.
	Applied bool `json:"applied"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (c *HarnessTurnCancel) UnmarshalJSON(data []byte) error {
	type plain HarnessTurnCancel
	if err := relayRejectUnknownFields(data, reflect.TypeFor[plain](), ""); err != nil {
		return err
	}
	return json.Unmarshal(data, (*plain)(c))
}

type HandledRelayCommand struct {
	Command          RelayCommand                  `json:"command,omitempty"`
	TerminalIdentity *TerminalRelayCommandIdentity `json:"terminal_identity,omitempty"`
	AcceptedOrdinal  uint64                        `json:"accepted_ordinal"`
	TerminalOrdinal  *uint64                       `json:"terminal_ordinal"`
	Outcome          RelayCommandOutcome           `json:"outcome,omitempty"`
	Failure          *string                       `json:"failure,omitempty"`
}

// UnmarshalJSON decodes Command and Outcome through their variant tables.
func (h *HandledRelayCommand) UnmarshalJSON(data []byte) error {
	type plain HandledRelayCommand
	aux := struct {
		*plain
		Command json.RawMessage `json:"command"`
		Outcome json.RawMessage `json:"outcome"`
	}{plain: (*plain)(h)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	h.Command, h.Outcome = nil, nil
	if len(aux.Command) > 0 && string(aux.Command) != "null" {
		command, err := UnmarshalRelayCommandJSON(aux.Command)
		if err != nil {
			return err
		}
		h.Command = command
	}
	if len(aux.Outcome) > 0 && string(aux.Outcome) != "null" {
		outcome, err := UnmarshalRelayCommandOutcomeJSON(aux.Outcome)
		if err != nil {
			return err
		}
		h.Outcome = outcome
	}
	return nil
}

// TerminalRelayCommandIdentity is the information a terminal receipt needs
// after its execution payload is gone.
type TerminalRelayCommandIdentity struct {
	Digest               string  `json:"digest"`
	MailboxEventKey      *string `json:"mailbox_event_key,omitempty"`
	Prompt               bool    `json:"prompt"`
	CancelsCapacityRetry bool    `json:"cancels_capacity_retry"`
	ReleasesHistory      bool    `json:"releases_history"`
}

// MatchesCommand: mailbox keys preserve event identity across
// plain-text/structured retries.
func (h *HandledRelayCommand) MatchesCommand(command RelayCommand) (bool, error) {
	panic("unported: mj-core/src/relay/snapshot.rs HandledRelayCommand::matches_command")
}

func relayCommandDigest(command RelayCommand) (string, error) {
	// Object insertion order is not part of command equality, including ACP
	// extension metadata. Go's encoder sorts map keys, so a round trip
	// through generic values sorts every object.
	encoded, err := relayMarshalJSON(command)
	if err != nil {
		return "", err
	}
	decoder := json.NewDecoder(bytes.NewReader(encoded))
	decoder.UseNumber()
	var value any
	if err := decoder.Decode(&value); err != nil {
		return "", err
	}
	if encoded, err = relayMarshalJSON(value); err != nil {
		return "", err
	}
	digest := sha256.Sum256(encoded)
	return LowerHex(digest[:]), nil
}

// HandbackChecks is worker-owned assignment identity and evidence, replayed
// from delivery events.
type HandbackChecks struct {
	AssignmentID *string `json:"assignment_id"`
	// Sorted and unique (a Rust BTreeSet).
	Checked []string `json:"checked"`
	// Instructions only, allocated on the first delivered parent assignment.
	Context *ContextHistory `json:"context,omitempty"`
}

// NativeSessionUseEvidence is durable native-history proof, independent of
// the journal's retained prefix. Unknown fields are refused.
type NativeSessionUseEvidence struct {
	Version         uint32           `json:"version"`
	NativeSessionID string           `json:"native_session_id"`
	OpenedOrdinal   *uint64          `json:"opened_ordinal"`
	Usage           NativeSessionUse `json:"usage"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (e *NativeSessionUseEvidence) UnmarshalJSON(data []byte) error {
	type plain NativeSessionUseEvidence
	if err := relayRejectUnknownFields(data, reflect.TypeFor[plain](), ""); err != nil {
		return err
	}
	return json.Unmarshal(data, (*plain)(e))
}

type NativeSessionUse string

const (
	NativeSessionUseUnknown NativeSessionUse = "unknown"
	NativeSessionUseUnused  NativeSessionUse = "unused"
	NativeSessionUseUsed    NativeSessionUse = "used"
)

// RelaySnapshot is the relay's durable private state. Unknown fields are
// refused. Maps and slices without omitempty must be non-nil to encode as {}
// or [] rather than null; NewRelaySnapshot makes them so.
type RelaySnapshot struct {
	// Final reply of the latest completed turn, retained for its late assessment.
	CompletedReplyMessage *string `json:"completed_reply_message,omitempty"`
	ManagedSubagent       bool    `json:"managed_subagent"`
	// Whether the work processes the latest finished turn left running are
	// still alive. Owned by the worker (see WorkLiveness). Set when a turn
	// completes with live process identities, cleared when the next prompt is
	// accepted.
	WorkLiveness *WorkLiveness `json:"work_liveness,omitempty"`
	// Facts of the latest autonomous harness turn of a managed child.
	HarnessTurnWork *HarnessTurnWork `json:"harness_turn_work,omitempty"`
	HandbackChecks  HandbackChecks   `json:"handback_checks"`
	// Survives queue removal and receipt GC, fencing reminders for older turns.
	LatestPromptAcceptedOrdinal *uint64 `json:"latest_prompt_accepted_ordinal,omitempty"`
	// Sorted and unique (a Rust BTreeSet).
	AssessmentQuestions []string `json:"assessment_questions"`
	// Worker-owned form bodies survive acknowledgement-based journal GC.
	// A nil body preserves an open legacy ID whose event was already pruned.
	PendingElicitations map[string]*ElicitationRequest `json:"pending_elicitations,omitempty"`
	Assessment          *TurnAssessment                `json:"assessment,omitempty"`
	// Latest input-requiring completion that the person has yet to address.
	Request           *OpenRequest      `json:"request,omitempty"`
	NotificationStack NotificationStack `json:"notification_stack"`
	// Acknowledged completions stay pruned even if their assessments arrive later.
	NotificationsReadThrough uint64 `json:"notifications_read_through"`
	// Journal ordinal of the latest person input; only snapshot apply advances it.
	PersonInputThrough uint64          `json:"person_input_through"`
	AssessmentContext  *ContextHistory `json:"assessment_context,omitempty"`
	// The latest prompt's completion identity; see TurnCompletion.
	TurnCompletion     *TurnCompletion        `json:"turn_completion,omitempty"`
	Continuation       ContinuationState      `json:"continuation"`
	Steering           *SteeringOperation     `json:"steering,omitempty"`
	CancellingPromptID *string                `json:"cancelling_prompt_id,omitempty"`
	NativeAgents       map[string]NativeAgent `json:"native_agents,omitempty"`
	// Nil means None (serialized as null); an empty non-nil map is {}.
	NativeAgentReplay       map[string]NativeAgent `json:"native_agent_replay"`
	Goal                    GoalState              `json:"goal"`
	CapacityRetry           *CapacityRetry         `json:"capacity_retry,omitempty"`
	RetryAssessment         *RetryAssessment       `json:"retry_assessment,omitempty"`
	ActivityTurnStartedAtMs *int64                 `json:"activity_turn_started_at_ms,omitempty"`
	StoreID                 *string                `json:"store_id,omitempty"`
	FormatVersion           uint32                 `json:"format_version"`
	IdleSinceMs             *int64                 `json:"idle_since_ms,omitempty"`
	ActivityWasIdle         *bool                  `json:"activity_was_idle,omitempty"`
	SessionID               string                 `json:"session_id"`
	Execution               RelayExecutionState    `json:"execution"`
	LatestOrdinal           uint64                 `json:"latest_ordinal"`
	LatestDigest            string                 `json:"latest_digest"`
	AcknowledgedThrough     uint64                 `json:"acknowledged_through"`
	AcknowledgedDigest      string                 `json:"acknowledged_digest"`
	RecoveryFloorOrdinal    uint64                 `json:"recovery_floor_ordinal"`
	RecoveryFloorDigest     string                 `json:"recovery_floor_digest"`
	NativeSessionID         *string                `json:"native_session_id"`
	// Persisted alongside the native id so a controller that reconnects
	// after the fallback still sees that continuity was lost.
	NativeContinuityLost bool `json:"native_continuity_lost"`
	// The native session NativeSessionID replaced because it was never
	// used. Cleared by the next open or clear. Written only while set, so an
	// older worker still reads every other snapshot.
	ReplacedUnusedNativeSessionID *string `json:"replaced_unused_native_session_id,omitempty"`
	// The native thread behind NativeSessionID has been used: a prompt
	// was transmitted to it, Claude Code reported a model cycle's result, it
	// was resumed rather than created here, or its identity arrived from
	// outside this journal. Codex writes a thread's rollout only at its first
	// user message, so an unused thread can be missing on disk and safely
	// replaced; a used one cannot. Older snapshots omit the field and read as
	// false, and it is written only once true so an older worker keeps
	// reading the snapshots of sessions that never used their thread.
	NativeSessionUsed bool                      `json:"native_session_used,omitempty"`
	NativeUseEvidence *NativeSessionUseEvidence `json:"native_use_evidence,omitempty"`
	// Accepted command receipts retained until their durable operation
	// finishes. Each set is sorted and unique.
	CommandRetention map[string][]string `json:"command_retention,omitempty"`
	// Accepted startup identities, including migrated pins without receipt
	// bodies. Seeding consults this lifetime set even after context clear.
	// Sorted and unique.
	InitialCommandIDs []string `json:"initial_command_ids,omitempty"`
	// Upgrade protection until the daemon reconciles the outstanding Move.
	LegacyCommandRetention bool `json:"legacy_command_retention,omitempty"`
	// Ordinal of the SessionOpened that recorded NativeSessionID. It
	// places the current native session relative to RecoveryFloorOrdinal:
	// a session opened above the floor has everything about it recorded in
	// the events this snapshot still describes, so a released archive below
	// the floor cannot hide history belonging to it. Older snapshots omit the
	// field and read as nil, which is treated as unknown, and it is
	// written only once a session has been opened, so an older worker keeps
	// reading the snapshots of sessions that never opened one.
	NativeSessionOpenedOrdinal *uint64 `json:"native_session_opened_ordinal,omitempty"`
	// This relay was restored from a checkpoint whose conversation shows the
	// native session it continues never received a prompt
	// (RestoredRelaySeed.NativeSessionUnused). The identity arrives with
	// the launch configuration rather than from this journal, and this is the
	// evidence that it is unused. The first SessionOpened clears it, since
	// that opening then places the session against the recovery floor.
	// Written only while true, so an older worker still reads every other
	// snapshot.
	RestoredNativeSessionUnused bool                            `json:"restored_native_session_unused,omitempty"`
	AgentCapabilities           *acpschema.AgentCapabilities    `json:"agent_capabilities"`
	AgentInfo                   *acpschema.Implementation       `json:"agent_info"`
	Runtime                     *RuntimeReceipt                 `json:"runtime,omitempty"`
	ConfigOptions               []acpschema.SessionConfigOption `json:"config_options"`
	Modes                       *acpschema.SessionModeState     `json:"modes,omitempty"`
	AvailableCommands           []acpschema.AvailableCommand    `json:"available_commands"`
	Config                      map[string]string               `json:"config"`
	ActivePrompt                *StoredActiveRelayPrompt        `json:"active_prompt"`
	QueuedPrompts               []StoredQueuedRelayCommand      `json:"queued_prompts"`
	PendingPromptContext        *PendingPromptContext           `json:"pending_prompt_context,omitempty"`
	PendingUserShellContexts    []PendingUserShellContext       `json:"pending_user_shell_contexts,omitempty"`
	// Events waiting for a hook, prompt, or idle wake to claim them.
	PendingMailboxEvents []MailboxEvent `json:"pending_mailbox_events,omitempty"`
	// Events exposed to a hook whose output has not yet been acknowledged.
	MailboxHookLease *MailboxHookLease `json:"mailbox_hook_lease,omitempty"`
	// Recently delivered event keys and their journal ordinals. Pending keys
	// remain represented by PendingMailboxEvents until delivery.
	DeliveredMailboxEventKeys map[string]uint64 `json:"delivered_mailbox_event_keys,omitempty"`
	// Mailbox context already claimed for a prompt that has not yet reached
	// a terminal dispatch state. This closes the journal/snapshot crash gap.
	MailboxPromptContexts map[string]string          `json:"mailbox_prompt_contexts,omitempty"`
	ActiveUserShells      map[string]ActiveUserShell `json:"active_user_shells,omitempty"`
	// Ignored. Protocol 26 workers persisted a Move seal here; the field
	// stays so their stores still open after an upgrade.
	CommandLedgerSeal             *string            `json:"command_ledger_seal,omitempty"`
	CheckpointBarrier             *string            `json:"checkpoint_barrier"`
	CheckpointReadyThrough        *uint64            `json:"checkpoint_ready_through"`
	CheckpointReadyDigest         *string            `json:"checkpoint_ready_digest"`
	HarnessTurn                   *StoredHarnessTurn `json:"harness_turn,omitempty"`
	LastHarnessTurnStartedOrdinal *uint64            `json:"last_harness_turn_started_ordinal,omitempty"`
	// The stop aimed at the open autonomous harness turn, while that turn is
	// open. Set when a Cancel or CancelTurn is accepted with no prompt of
	// ours running, so every settle the stop causes finds it however the
	// harness's result and the acknowledgement race. Never serialized once
	// the turn settles.
	CancellingHarnessTurn *HarnessTurnCancel `json:"cancelling_harness_turn,omitempty"`
	// Ignored. Protocol 26 workers persisted durable command receipts here;
	// the field stays so their stores still open after an upgrade.
	RetainedCommandReceipts []string `json:"retained_command_receipts,omitempty"`
	// Ignored, kept for the same reason as RetainedCommandReceipts.
	CancelledCommandAdmissions []string                       `json:"cancelled_command_admissions,omitempty"`
	HandledCommands            map[string]HandledRelayCommand `json:"handled_commands"`
	Dispatches                 map[string]RelayDispatchRecord `json:"dispatches"`
}

// UnmarshalJSON refuses unknown fields, as serde's deny_unknown_fields does.
func (s *RelaySnapshot) UnmarshalJSON(data []byte) error {
	type plain RelaySnapshot
	if err := relayRejectUnknownFields(data, reflect.TypeFor[plain](), ""); err != nil {
		return err
	}
	return json.Unmarshal(data, (*plain)(s))
}

type SettledHarnessTurn struct {
	StartOrdinal uint64 `json:"start_ordinal"`
	StopReason   string `json:"stop_reason"`
	// What a managed child's worker observed when this turn ended, as for a
	// prompt's outcome. Absent for other sessions and in older journals,
	// which keeps their digests.
	WorkFacts *WorkFacts `json:"work_facts,omitempty"`
}

// HarnessTurnWork is the facts a managed child's worker observed when its
// latest autonomous harness turn ended. A prompted turn's facts live in its
// handled command; this is the same record for a turn no prompt of ours
// started, so the worker's liveness watcher can recount its processes.
type HarnessTurnWork struct {
	// The settle event's ordinal: the completed_ordinal of the turn.
	CompletedOrdinal uint64    `json:"completed_ordinal"`
	Facts            WorkFacts `json:"facts"`
}

// AuthorizationContext is the stored assessment history as readers must see
// it: without the user messages that today's rules attribute to Mjolnir. The
// history is stored as written so journal digests stay valid; every reader
// that hands it to Jev or the question locator goes through here.
func (s *RelaySnapshot) AuthorizationContext() *ContextHistory {
	panic("unported: mj-core/src/relay/snapshot.rs RelaySnapshot::authorization_context")
}

// AutonomousHarnessTurn is the open turn that began without a prompt.
func (s *RelaySnapshot) AutonomousHarnessTurn() *StoredHarnessTurn {
	if s.HarnessTurn != nil && s.HarnessTurn.Autonomous {
		turn := *s.HarnessTurn
		return &turn
	}
	return nil
}

func NewRelaySnapshot(sessionID string) RelaySnapshot {
	panic("unported: mj-core/src/relay/snapshot.rs RelaySnapshot::new")
}

// ClearContextInProgress reports whether a /clear is still waiting or
// running. Terminal records stay in the ledger until the daemon acknowledges
// their events, so they are history, not work: a completed, rejected or
// interrupted clear must not hold later work back.
func (s *RelaySnapshot) ClearContextInProgress() bool {
	for _, dispatch := range s.Dispatches {
		if _, clear := dispatch.Command.(RelayCommandClearContext); clear {
			switch dispatch.State {
			case RelayDispatchStateQueued, RelayDispatchStatePending, RelayDispatchStateInFlight:
				return true
			}
		}
	}
	return false
}

// ClearContextStartedAtMs is when the in-progress /clear began, which
// activity treats as a turn.
func (s *RelaySnapshot) ClearContextStartedAtMs() *int64 {
	if s.ClearContextInProgress() {
		return s.ActivityTurnStartedAtMs
	}
	return nil
}

// OpenRequest: semantic input is derived from the request and the person's
// durable frontier.
func (s *RelaySnapshot) OpenRequest() *OpenRequest {
	panic("unported: mj-core/src/relay/snapshot.rs RelaySnapshot::open_request")
}

// ValidateNotificationCommand is used at command admission and during
// journal replay. The error text is the refusal shown to the person.
func (s *RelaySnapshot) ValidateNotificationCommand(command RelayCommand) error {
	panic("unported: mj-core/src/relay/snapshot.rs RelaySnapshot::validate_notification_command")
}

// RestartContinuationID chooses recovery identity from this ledger so
// occupied generations cannot reject a restart transfer. Replay sees the same
// ledger and choice.
func (s *RelaySnapshot) RestartContinuationID(commandID string) string {
	panic("unported: mj-core/src/relay/snapshot.rs RelaySnapshot::restart_continuation_id")
}

func (s *RelaySnapshot) OperationalState() RelayOperationalState {
	panic("unported: mj-core/src/relay/snapshot.rs RelaySnapshot::operational_state")
}

func (s *RelaySnapshot) RetainedThrough() uint64 {
	return retainedEventOrdinal(RelayProtocolVersion, s.AcknowledgedThrough, s.RecoveryFloorOrdinal)
}

func (s *RelaySnapshot) RetainedDigest() string {
	return s.AcknowledgedDigest
}

func (s *RelaySnapshot) RecordNativeUse(usage NativeSessionUse) {
	if s.NativeSessionID != nil {
		s.NativeUseEvidence = &NativeSessionUseEvidence{
			Version:         1,
			NativeSessionID: *s.NativeSessionID,
			OpenedOrdinal:   s.NativeSessionOpenedOrdinal,
			Usage:           usage,
		}
	}
}

func retainedEventOrdinal(protocol uint32, acknowledged, recoveryFloor uint64) uint64 {
	if protocol < RelayCommandRetentionProtocol {
		return min(acknowledged, recoveryFloor)
	}
	return acknowledged
}

// RelayCommand is a durable command submitted to the relay.
type RelayCommand interface {
	json.Marshaler
	isRelayCommand()
}

// Durable installation of hidden context before the first prompt.
type RelayCommandInstallPromptContext struct {
	Text string `json:"text"`
}

func (RelayCommandInstallPromptContext) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandInstallPromptContext.
func (v RelayCommandInstallPromptContext) MarshalJSON() ([]byte, error) {
	type plain RelayCommandInstallPromptContext
	return relayAdjacentJSON("type", "data", "install_prompt_context", plain(v))
}

// Seed exact retained authorization history at the controller's verified frontier.
type RelayCommandSeedAssessmentContext struct {
	Expected RelayCursor    `json:"expected"`
	Context  ContextHistory `json:"context"`
}

func (RelayCommandSeedAssessmentContext) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandSeedAssessmentContext.
func (v RelayCommandSeedAssessmentContext) MarshalJSON() ([]byte, error) {
	type plain RelayCommandSeedAssessmentContext
	return relayAdjacentJSON("type", "data", "seed_assessment_context", plain(v))
}

// Dismiss only the inferred question belonging to this completed turn.
type RelayCommandDismissQuestion struct {
	Question InferredQuestion `json:"question"`
}

func (RelayCommandDismissQuestion) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandDismissQuestion.
func (v RelayCommandDismissQuestion) MarshalJSON() ([]byte, error) {
	type plain RelayCommandDismissQuestion
	return relayAdjacentJSON("type", "data", "dismiss_question", plain(v))
}

// Clear the stack observed at this exact revision.
type RelayCommandClearNotifications struct {
	Revision uint64 `json:"revision"`
}

func (RelayCommandClearNotifications) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandClearNotifications.
func (v RelayCommandClearNotifications) MarshalJSON() ([]byte, error) {
	type plain RelayCommandClearNotifications
	return relayAdjacentJSON("type", "data", "clear_notifications", plain(v))
}

// Reclaim Important entries covered by the daemon's durable read cursor.
type RelayCommandNotificationsRead struct {
	Through uint64 `json:"through"`
}

func (RelayCommandNotificationsRead) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandNotificationsRead.
func (v RelayCommandNotificationsRead) MarshalJSON() ([]byte, error) {
	type plain RelayCommandNotificationsRead
	return relayAdjacentJSON("type", "data", "notifications_read", plain(v))
}

// Start an empty native conversation, preserving the logical session.
type RelayCommandClearContext struct{}

func (RelayCommandClearContext) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandClearContext.
func (v RelayCommandClearContext) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "clear_context", nil)
}

type RelayCommandPrompt struct {
	Prompt []acpschema.ContentBlock `json:"prompt"`
}

func (RelayCommandPrompt) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandPrompt.
func (v RelayCommandPrompt) MarshalJSON() ([]byte, error) {
	type plain RelayCommandPrompt
	return relayAdjacentJSON("type", "data", "prompt", plain(v))
}

// Legacy protocol-39 command, decoded and replayed without changing state.
type RelayCommandSetSubagentWait struct {
	ActivePromptID string `json:"active_prompt_id"`
	Waiting        bool   `json:"waiting"`
}

func (RelayCommandSetSubagentWait) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandSetSubagentWait.
func (v RelayCommandSetSubagentWait) MarshalJSON() ([]byte, error) {
	type plain RelayCommandSetSubagentWait
	return relayAdjacentJSON("type", "data", "set_subagent_wait", plain(v))
}

// Deliver the fixed reminder only while this completed turn is still current.
type RelayCommandHandbackReminder struct {
	CompletedCommandID string `json:"completed_command_id"`
	CompletedOrdinal   uint64 `json:"completed_ordinal"`
	// Rendered lines for work processes the child left running. Empty
	// gives the fixed text. Only sent to workers at protocol 67 or later:
	// older workers reject unknown fields.
	Running []string `json:"running,omitempty"`
}

func (RelayCommandHandbackReminder) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandHandbackReminder.
func (v RelayCommandHandbackReminder) MarshalJSON() ([]byte, error) {
	type plain RelayCommandHandbackReminder
	return relayAdjacentJSON("type", "data", "handback_reminder", plain(v))
}

// Prompt a child Mjolnir is holding for its own background work, only
// while this completed turn (the reminder turn) is still current.
type RelayCommandHoldFollowUp struct {
	CompletedCommandID string           `json:"completed_command_id"`
	CompletedOrdinal   uint64           `json:"completed_ordinal"`
	Kind               HoldFollowUpKind `json:"kind"`
	Running            []string         `json:"running"`
}

func (RelayCommandHoldFollowUp) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandHoldFollowUp.
func (v RelayCommandHoldFollowUp) MarshalJSON() ([]byte, error) {
	type plain RelayCommandHoldFollowUp
	return relayAdjacentJSON("type", "data", "hold_follow_up", plain(v))
}

// Add one event to the durable mailbox. Pending keys and recently
// delivered keys are deduplicated by the relay.
type RelayCommandDeliverMailboxEvent struct {
	Event MailboxEvent `json:"event"`
}

func (RelayCommandDeliverMailboxEvent) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandDeliverMailboxEvent.
func (v RelayCommandDeliverMailboxEvent) MarshalJSON() ([]byte, error) {
	type plain RelayCommandDeliverMailboxEvent
	return relayAdjacentJSON("type", "data", "deliver_mailbox_event", plain(v))
}

// A worker-created prompt that carries mailbox events selected at an
// idle turn boundary. Queueing this command is the durable wake claim.
type RelayCommandMailboxWake struct {
	Events []MailboxEvent `json:"events"`
}

func (RelayCommandMailboxWake) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandMailboxWake.
func (v RelayCommandMailboxWake) MarshalJSON() ([]byte, error) {
	type plain RelayCommandMailboxWake
	return relayAdjacentJSON("type", "data", "mailbox_wake", plain(v))
}

// A fixed prompt admitted only against the exact classified state.
type RelayCommandContinueAuthorizedWork struct {
	Expected           RelayCursor `json:"expected"`
	UserCommandID      string      `json:"user_command_id"`
	CompletedCommandID string      `json:"completed_command_id"`
	Attempt            uint8       `json:"attempt"`
}

func (RelayCommandContinueAuthorizedWork) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandContinueAuthorizedWork.
func (v RelayCommandContinueAuthorizedWork) MarshalJSON() ([]byte, error) {
	type plain RelayCommandContinueAuthorizedWork
	return relayAdjacentJSON("type", "data", "continue_authorized_work", plain(v))
}

type RelayCommandSetQuotaRecovery struct {
	Expected RelayCursor    `json:"expected"`
	Recovery *QuotaRecovery `json:"recovery"`
}

func (RelayCommandSetQuotaRecovery) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandSetQuotaRecovery.
func (v RelayCommandSetQuotaRecovery) MarshalJSON() ([]byte, error) {
	type plain RelayCommandSetQuotaRecovery
	return relayAdjacentJSON("type", "data", "set_quota_recovery", plain(v))
}

type RelayCommandResumeAfterQuota struct {
	Expected           RelayCursor `json:"expected"`
	CompletedCommandID string      `json:"completed_command_id"`
}

func (RelayCommandResumeAfterQuota) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandResumeAfterQuota.
func (v RelayCommandResumeAfterQuota) MarshalJSON() ([]byte, error) {
	type plain RelayCommandResumeAfterQuota
	return relayAdjacentJSON("type", "data", "resume_after_quota", plain(v))
}

type RelayCommandRunUserShell struct {
	Command string `json:"command"`
}

func (RelayCommandRunUserShell) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandRunUserShell.
func (v RelayCommandRunUserShell) MarshalJSON() ([]byte, error) {
	type plain RelayCommandRunUserShell
	return relayAdjacentJSON("type", "data", "run_user_shell", plain(v))
}

type RelayCommandCancelUserShell struct {
	ShellCommandID string `json:"shell_command_id"`
}

func (RelayCommandCancelUserShell) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandCancelUserShell.
func (v RelayCommandCancelUserShell) MarshalJSON() ([]byte, error) {
	type plain RelayCommandCancelUserShell
	return relayAdjacentJSON("type", "data", "cancel_user_shell", plain(v))
}

type RelayCommandRemoveQueuedPrompt struct {
	QueuedCommandID string `json:"queued_command_id"`
}

func (RelayCommandRemoveQueuedPrompt) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandRemoveQueuedPrompt.
func (v RelayCommandRemoveQueuedPrompt) MarshalJSON() ([]byte, error) {
	type plain RelayCommandRemoveQueuedPrompt
	return relayAdjacentJSON("type", "data", "remove_queued_prompt", plain(v))
}

type RelayCommandClearQueuedPrompts struct{}

func (RelayCommandClearQueuedPrompts) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandClearQueuedPrompts.
func (v RelayCommandClearQueuedPrompts) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "clear_queued_prompts", nil)
}

type RelayCommandSetConfig struct {
	Key   string `json:"key"`
	Value string `json:"value"`
}

func (RelayCommandSetConfig) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandSetConfig.
func (v RelayCommandSetConfig) MarshalJSON() ([]byte, error) {
	type plain RelayCommandSetConfig
	return relayAdjacentJSON("type", "data", "set_config", plain(v))
}

// Restore the worker's saved execution policy after planning.
type RelayCommandRestoreExecutionMode struct{}

func (RelayCommandRestoreExecutionMode) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandRestoreExecutionMode.
func (v RelayCommandRestoreExecutionMode) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "restore_execution_mode", nil)
}

// Native goal control delivered independently of the prompt queue.
type RelayCommandGoalControl struct {
	Action GoalControlAction `json:"action"`
}

func (RelayCommandGoalControl) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandGoalControl.
func (v RelayCommandGoalControl) MarshalJSON() ([]byte, error) {
	type plain RelayCommandGoalControl
	return relayAdjacentJSON("type", "data", "goal_control", plain(v))
}

// Opaque ACP session/set_mode id. Hel uses it for harnesses whose plan
// mode is a session mode rather than an advertised slash command.
type RelayCommandSetSessionMode struct {
	ModeID string `json:"mode_id"`
}

func (RelayCommandSetSessionMode) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandSetSessionMode.
func (v RelayCommandSetSessionMode) MarshalJSON() ([]byte, error) {
	type plain RelayCommandSetSessionMode
	return relayAdjacentJSON("type", "data", "set_session_mode", plain(v))
}

// Cancel the current turn without consuming a queued prompt. This is
// separate from RelayCommandCancel, whose UI semantics may steer
// the next queued prompt into the running turn.
type RelayCommandCancelTurn struct{}

func (RelayCommandCancelTurn) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandCancelTurn.
func (v RelayCommandCancelTurn) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "cancel_turn", nil)
}

// Steer exactly this queued prompt into exactly this running turn.
type RelayCommandSteer struct {
	ActivePromptID string `json:"active_prompt_id"`
	QueuedPromptID string `json:"queued_prompt_id"`
}

func (RelayCommandSteer) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandSteer.
func (v RelayCommandSteer) MarshalJSON() ([]byte, error) {
	type plain RelayCommandSteer
	return relayAdjacentJSON("type", "data", "steer", plain(v))
}

// Explicit cancellation cannot drift onto a subsequent turn.
type RelayCommandCancelTurnFor struct {
	ActivePromptID string `json:"active_prompt_id"`
}

func (RelayCommandCancelTurnFor) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandCancelTurnFor.
func (v RelayCommandCancelTurnFor) MarshalJSON() ([]byte, error) {
	type plain RelayCommandCancelTurnFor
	return relayAdjacentJSON("type", "data", "cancel_turn_for", plain(v))
}

// Interrupt this turn only while the intended queued change is still first.
type RelayCommandApplyQueuedFor struct {
	ActivePromptID  string `json:"active_prompt_id"`
	QueuedCommandID string `json:"queued_command_id"`
}

func (RelayCommandApplyQueuedFor) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandApplyQueuedFor.
func (v RelayCommandApplyQueuedFor) MarshalJSON() ([]byte, error) {
	type plain RelayCommandApplyQueuedFor
	return relayAdjacentJSON("type", "data", "apply_queued_for", plain(v))
}

// Release held input only after the user reviews uncertain delivery.
type RelayCommandResolveSteering struct {
	SteeringID string `json:"steering_id"`
}

func (RelayCommandResolveSteering) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandResolveSteering.
func (v RelayCommandResolveSteering) MarshalJSON() ([]byte, error) {
	type plain RelayCommandResolveSteering
	return relayAdjacentJSON("type", "data", "resolve_steering", plain(v))
}

type RelayCommandCancel struct{}

func (RelayCommandCancel) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandCancel.
func (v RelayCommandCancel) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "cancel", nil)
}

type RelayCommandClose struct {
	BarrierCommandID string      `json:"barrier_command_id"`
	Expected         RelayCursor `json:"expected"`
}

func (RelayCommandClose) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandClose.
func (v RelayCommandClose) MarshalJSON() ([]byte, error) {
	type plain RelayCommandClose
	return relayAdjacentJSON("type", "data", "close", plain(v))
}

type RelayCommandBeginCheckpoint struct {
	Reason *string `json:"reason"`
}

func (RelayCommandBeginCheckpoint) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandBeginCheckpoint.
func (v RelayCommandBeginCheckpoint) MarshalJSON() ([]byte, error) {
	type plain RelayCommandBeginCheckpoint
	return relayAdjacentJSON("type", "data", "begin_checkpoint", plain(v))
}

type RelayCommandCompleteCheckpoint struct {
	BarrierCommandID string `json:"barrier_command_id"`
}

func (RelayCommandCompleteCheckpoint) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandCompleteCheckpoint.
func (v RelayCommandCompleteCheckpoint) MarshalJSON() ([]byte, error) {
	type plain RelayCommandCompleteCheckpoint
	return relayAdjacentJSON("type", "data", "complete_checkpoint", plain(v))
}

// Resume ACP dispatch for a barrier whose archive is exported but not yet
// installed on the controller. The archive-coverage marker stays put;
// current workers retain journal history through daemon acknowledgement.
type RelayCommandReleaseCheckpoint struct {
	BarrierCommandID string `json:"barrier_command_id"`
}

func (RelayCommandReleaseCheckpoint) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandReleaseCheckpoint.
func (v RelayCommandReleaseCheckpoint) MarshalJSON() ([]byte, error) {
	type plain RelayCommandReleaseCheckpoint
	return relayAdjacentJSON("type", "data", "release_checkpoint", plain(v))
}

// Move the recovery floor to a cursor that an installed archive covers.
// Valid with or without an active barrier.
type RelayCommandAdvanceRecoveryFloor struct {
	Through RelayCursor `json:"through"`
}

func (RelayCommandAdvanceRecoveryFloor) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandAdvanceRecoveryFloor.
func (v RelayCommandAdvanceRecoveryFloor) MarshalJSON() ([]byte, error) {
	type plain RelayCommandAdvanceRecoveryFloor
	return relayAdjacentJSON("type", "data", "advance_recovery_floor", plain(v))
}

// Put a controller-authored line into the conversation. The agent never
// sees it: it explains something Hel did to the session, such as moving
// its checkout, to the person reading the transcript.
type RelayCommandRecordNotice struct {
	Text string `json:"text"`
}

func (RelayCommandRecordNotice) isRelayCommand() {}

// MarshalJSON writes the serde-tagged form of RelayCommandRecordNotice.
func (v RelayCommandRecordNotice) MarshalJSON() ([]byte, error) {
	type plain RelayCommandRecordNotice
	return relayAdjacentJSON("type", "data", "record_notice", plain(v))
}

// relayCommandVariants decodes each RelayCommand variant by its wire name.
var relayCommandVariants = map[string]func(json.RawMessage) (RelayCommand, error){
	"install_prompt_context":   relayVariant[RelayCommandInstallPromptContext, RelayCommand](true),
	"seed_assessment_context":  relayVariant[RelayCommandSeedAssessmentContext, RelayCommand](true),
	"dismiss_question":         relayVariant[RelayCommandDismissQuestion, RelayCommand](true),
	"clear_notifications":      relayVariant[RelayCommandClearNotifications, RelayCommand](true),
	"notifications_read":       relayVariant[RelayCommandNotificationsRead, RelayCommand](true),
	"clear_context":            relayVariant[RelayCommandClearContext, RelayCommand](true),
	"prompt":                   relayVariant[RelayCommandPrompt, RelayCommand](true),
	"set_subagent_wait":        relayVariant[RelayCommandSetSubagentWait, RelayCommand](true),
	"handback_reminder":        relayVariant[RelayCommandHandbackReminder, RelayCommand](true),
	"hold_follow_up":           relayVariant[RelayCommandHoldFollowUp, RelayCommand](true),
	"deliver_mailbox_event":    relayVariant[RelayCommandDeliverMailboxEvent, RelayCommand](true),
	"mailbox_wake":             relayVariant[RelayCommandMailboxWake, RelayCommand](true),
	"continue_authorized_work": relayVariant[RelayCommandContinueAuthorizedWork, RelayCommand](true),
	"set_quota_recovery":       relayVariant[RelayCommandSetQuotaRecovery, RelayCommand](true),
	"resume_after_quota":       relayVariant[RelayCommandResumeAfterQuota, RelayCommand](true),
	"run_user_shell":           relayVariant[RelayCommandRunUserShell, RelayCommand](true),
	"cancel_user_shell":        relayVariant[RelayCommandCancelUserShell, RelayCommand](true),
	"remove_queued_prompt":     relayVariant[RelayCommandRemoveQueuedPrompt, RelayCommand](true),
	"clear_queued_prompts":     relayVariant[RelayCommandClearQueuedPrompts, RelayCommand](true),
	"set_config":               relayVariant[RelayCommandSetConfig, RelayCommand](true),
	"restore_execution_mode":   relayVariant[RelayCommandRestoreExecutionMode, RelayCommand](true),
	"goal_control":             relayVariant[RelayCommandGoalControl, RelayCommand](true),
	"set_session_mode":         relayVariant[RelayCommandSetSessionMode, RelayCommand](true),
	"cancel_turn":              relayVariant[RelayCommandCancelTurn, RelayCommand](true),
	"steer":                    relayVariant[RelayCommandSteer, RelayCommand](true),
	"cancel_turn_for":          relayVariant[RelayCommandCancelTurnFor, RelayCommand](true),
	"apply_queued_for":         relayVariant[RelayCommandApplyQueuedFor, RelayCommand](true),
	"resolve_steering":         relayVariant[RelayCommandResolveSteering, RelayCommand](true),
	"cancel":                   relayVariant[RelayCommandCancel, RelayCommand](true),
	"close":                    relayVariant[RelayCommandClose, RelayCommand](true),
	"begin_checkpoint":         relayVariant[RelayCommandBeginCheckpoint, RelayCommand](true),
	"complete_checkpoint":      relayVariant[RelayCommandCompleteCheckpoint, RelayCommand](true),
	"release_checkpoint":       relayVariant[RelayCommandReleaseCheckpoint, RelayCommand](true),
	"advance_recovery_floor":   relayVariant[RelayCommandAdvanceRecoveryFloor, RelayCommand](true),
	"record_notice":            relayVariant[RelayCommandRecordNotice, RelayCommand](true),
}

// UnmarshalRelayCommandJSON decodes the serde-tagged form of RelayCommand.
func UnmarshalRelayCommandJSON(data []byte) (RelayCommand, error) {
	name, body, err := relaySplitAdjacent(data, "type", "data", true)
	if err != nil {
		return nil, fmt.Errorf("decode RelayCommand: %w", err)
	}
	decode, ok := relayCommandVariants[name]
	if !ok {
		return nil, fmt.Errorf("decode RelayCommand: unknown variant %q", name)
	}
	return decode(body)
}

// HarnessPreparation is how far the current worker process has got in
// preparing its harness.
//
// The worker serves its relay before this work starts, so a client can tell
// a worker that is still preparing from one that is not answering. The
// worker is the only owner of this fact; it belongs to one worker process
// and is never carried across a restart.
type HarnessPreparation interface {
	json.Marshaler
	isHarnessPreparation()
}

// Preparation is running Step (a worker-startup.json step name),
// which began at SinceMs.
type HarnessPreparationPreparing struct {
	Step    string `json:"step"`
	SinceMs int64  `json:"since_ms"`
}

func (HarnessPreparationPreparing) isHarnessPreparation() {}

// MarshalJSON writes the serde-tagged form of HarnessPreparationPreparing.
func (v HarnessPreparationPreparing) MarshalJSON() ([]byte, error) {
	type plain HarnessPreparationPreparing
	return relayInternalJSON("state", "preparing", plain(v))
}

// The harness bridge was started. ACPReady then reports whether its
// session is open.
type HarnessPreparationStarted struct{}

func (HarnessPreparationStarted) isHarnessPreparation() {}

// MarshalJSON writes the serde-tagged form of HarnessPreparationStarted.
func (v HarnessPreparationStarted) MarshalJSON() ([]byte, error) {
	return relayInternalJSON("state", "started", nil)
}

// Preparation stopped at Step. The worker keeps serving its relay, so
// the session can still be checkpointed and closed.
type HarnessPreparationFailed struct {
	Step  string `json:"step"`
	Error string `json:"error"`
	AtMs  int64  `json:"at_ms"`
}

func (HarnessPreparationFailed) isHarnessPreparation() {}

// MarshalJSON writes the serde-tagged form of HarnessPreparationFailed.
func (v HarnessPreparationFailed) MarshalJSON() ([]byte, error) {
	type plain HarnessPreparationFailed
	return relayInternalJSON("state", "failed", plain(v))
}

// harnessPreparationVariants decodes each HarnessPreparation variant by its wire name.
var harnessPreparationVariants = map[string]func(json.RawMessage) (HarnessPreparation, error){
	"preparing": relayInternalVariant[HarnessPreparationPreparing, HarnessPreparation]("state", false),
	"started":   relayInternalVariant[HarnessPreparationStarted, HarnessPreparation]("state", false),
	"failed":    relayInternalVariant[HarnessPreparationFailed, HarnessPreparation]("state", false),
}

// UnmarshalHarnessPreparationJSON decodes the serde-tagged form of HarnessPreparation.
func UnmarshalHarnessPreparationJSON(data []byte) (HarnessPreparation, error) {
	name, err := relaySplitInternal(data, "state")
	body := json.RawMessage(data)
	if err != nil {
		return nil, fmt.Errorf("decode HarnessPreparation: %w", err)
	}
	decode, ok := harnessPreparationVariants[name]
	if !ok {
		return nil, fmt.Errorf("decode HarnessPreparation: unknown variant %q", name)
	}
	return decode(body)
}

// RelayObservation is what one relay event records.
type RelayObservation interface {
	json.Marshaler
	isRelayObservation()
}

type RelayObservationQuestionTargetUpdated struct {
	Question        InferredQuestion `json:"question"`
	QuestionMessage string           `json:"question_message"`
}

func (RelayObservationQuestionTargetUpdated) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationQuestionTargetUpdated.
func (v RelayObservationQuestionTargetUpdated) MarshalJSON() ([]byte, error) {
	type plain RelayObservationQuestionTargetUpdated
	return relayAdjacentJSON("type", "data", "question_target_updated", plain(v))
}

type RelayObservationQuestionLocatorFinished struct {
	CompletedOrdinal uint64  `json:"completed_ordinal"`
	QuestionMessage  *string `json:"question_message"`
}

func (RelayObservationQuestionLocatorFinished) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationQuestionLocatorFinished.
func (v RelayObservationQuestionLocatorFinished) MarshalJSON() ([]byte, error) {
	type plain RelayObservationQuestionLocatorFinished
	return relayAdjacentJSON("type", "data", "question_locator_finished", plain(v))
}

type RelayObservationManagedSubagentConfigured struct {
	Managed bool `json:"managed"`
}

func (RelayObservationManagedSubagentConfigured) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationManagedSubagentConfigured.
func (v RelayObservationManagedSubagentConfigured) MarshalJSON() ([]byte, error) {
	type plain RelayObservationManagedSubagentConfigured
	return relayAdjacentJSON("type", "data", "managed_subagent_configured", plain(v))
}

// The worker's recount of how many of a finished turn's work processes
// are alive. Dropped when it names a turn other than the current one.
type RelayObservationWorkLivenessChanged struct {
	Liveness WorkLiveness `json:"liveness"`
}

func (RelayObservationWorkLivenessChanged) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationWorkLivenessChanged.
func (v RelayObservationWorkLivenessChanged) MarshalJSON() ([]byte, error) {
	type plain RelayObservationWorkLivenessChanged
	return relayAdjacentJSON("type", "data", "work_liveness_changed", plain(v))
}

type RelayObservationHandbackCheckStarted struct {
	AssignmentID string `json:"assignment_id"`
}

func (RelayObservationHandbackCheckStarted) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationHandbackCheckStarted.
func (v RelayObservationHandbackCheckStarted) MarshalJSON() ([]byte, error) {
	type plain RelayObservationHandbackCheckStarted
	return relayAdjacentJSON("type", "data", "handback_check_started", plain(v))
}

type RelayObservationTurnAssessmentUpdated struct {
	Assessment TurnAssessment `json:"assessment"`
}

func (RelayObservationTurnAssessmentUpdated) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationTurnAssessmentUpdated.
func (v RelayObservationTurnAssessmentUpdated) MarshalJSON() ([]byte, error) {
	type plain RelayObservationTurnAssessmentUpdated
	return relayAdjacentJSON("type", "data", "turn_assessment_updated", plain(v))
}

type RelayObservationSteeringUnconfirmed struct {
	CommandID string `json:"command_id"`
	Message   string `json:"message"`
}

func (RelayObservationSteeringUnconfirmed) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationSteeringUnconfirmed.
func (v RelayObservationSteeringUnconfirmed) MarshalJSON() ([]byte, error) {
	type plain RelayObservationSteeringUnconfirmed
	return relayAdjacentJSON("type", "data", "steering_unconfirmed", plain(v))
}

type RelayObservationNativeAgent struct {
	Event NativeAgentEvent `json:"event"`
}

func (RelayObservationNativeAgent) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationNativeAgent.
func (v RelayObservationNativeAgent) MarshalJSON() ([]byte, error) {
	type plain RelayObservationNativeAgent
	return relayAdjacentJSON("type", "data", "native_agent", plain(v))
}

type RelayObservationAgentInitialized struct {
	ProtocolVersion acpschema.ProtocolVersion `json:"protocol_version"`
	// Serialized without session_capabilities.system_prompt: protocol 71 peers
	// hash the pre-systemPrompt AgentInitialized wire shape.
	Capabilities acpschema.AgentCapabilities `json:"capabilities"`
	AgentInfo    *acpschema.Implementation   `json:"agent_info"`
	Runtime      *RuntimeIdentity            `json:"runtime,omitempty"`
}

func (RelayObservationAgentInitialized) isRelayObservation() {}

type RelayObservationSessionOpened struct {
	NativeSessionID string `json:"native_session_id"`
	Resumed         bool   `json:"resumed"`
	// This session was opened fresh because the recorded native session
	// could not be reloaded. Older journals omit it, and false is never
	// written: the event digest covers this encoding, so emitting the
	// field for existing events would invalidate every journal on disk.
	NativeContinuityLost bool `json:"native_continuity_lost,omitempty"`
	// The native session this one replaced because the harness had no
	// record of it and this session never used it, so nothing was lost.
	// A resume that expected the replaced session accepts the new one on
	// this evidence alone. Absent on every other open, which keeps the
	// digests of existing records valid.
	ReplacedUnusedNativeSessionID *string `json:"replaced_unused_native_session_id,omitempty"`
}

func (RelayObservationSessionOpened) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationSessionOpened.
func (v RelayObservationSessionOpened) MarshalJSON() ([]byte, error) {
	type plain RelayObservationSessionOpened
	return relayAdjacentJSON("type", "data", "session_opened", plain(v))
}

type RelayObservationSessionConfigured struct {
	ConfigOptions []acpschema.SessionConfigOption `json:"config_options"`
}

func (RelayObservationSessionConfigured) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationSessionConfigured.
func (v RelayObservationSessionConfigured) MarshalJSON() ([]byte, error) {
	type plain RelayObservationSessionConfigured
	return relayAdjacentJSON("type", "data", "session_configured", plain(v))
}

type RelayObservationSessionModesConfigured struct {
	Modes *acpschema.SessionModeState `json:"modes"`
}

func (RelayObservationSessionModesConfigured) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationSessionModesConfigured.
func (v RelayObservationSessionModesConfigured) MarshalJSON() ([]byte, error) {
	type plain RelayObservationSessionModesConfigured
	return relayAdjacentJSON("type", "data", "session_modes_configured", plain(v))
}

type RelayObservationSessionUpdate struct {
	Update acpschema.SessionUpdate `json:"update"`
}

func (RelayObservationSessionUpdate) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationSessionUpdate.
func (v RelayObservationSessionUpdate) MarshalJSON() ([]byte, error) {
	type plain RelayObservationSessionUpdate
	return relayAdjacentJSON("type", "data", "session_update", plain(v))
}

type RelayObservationPermissionAutoApproved struct {
	OptionID   string `json:"option_id"`
	OptionName string `json:"option_name"`
}

func (RelayObservationPermissionAutoApproved) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationPermissionAutoApproved.
func (v RelayObservationPermissionAutoApproved) MarshalJSON() ([]byte, error) {
	type plain RelayObservationPermissionAutoApproved
	return relayAdjacentJSON("type", "data", "permission_auto_approved", plain(v))
}

type RelayObservationElicitationRequested struct {
	Request ElicitationRequest `json:"request"`
}

func (RelayObservationElicitationRequested) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationElicitationRequested.
func (v RelayObservationElicitationRequested) MarshalJSON() ([]byte, error) {
	type plain RelayObservationElicitationRequested
	return relayAdjacentJSON("type", "data", "elicitation_requested", plain(v))
}

type RelayObservationElicitationResolved struct {
	ElicitationID string `json:"elicitation_id"`
	Action        string `json:"action"`
	// Rendered user reply, with secret fields masked before recording.
	Reply *string `json:"reply,omitempty"`
}

func (RelayObservationElicitationResolved) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationElicitationResolved.
func (v RelayObservationElicitationResolved) MarshalJSON() ([]byte, error) {
	type plain RelayObservationElicitationResolved
	return relayAdjacentJSON("type", "data", "elicitation_resolved", plain(v))
}

type RelayObservationElicitationsCleared struct{}

func (RelayObservationElicitationsCleared) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationElicitationsCleared.
func (v RelayObservationElicitationsCleared) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "elicitations_cleared", nil)
}

type RelayObservationCommandQueued struct {
	CommandID   string       `json:"command_id"`
	Command     RelayCommand `json:"command"`
	CreatedAtMs int64        `json:"created_at_ms"`
}

func (RelayObservationCommandQueued) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationCommandQueued.
func (v RelayObservationCommandQueued) MarshalJSON() ([]byte, error) {
	type plain RelayObservationCommandQueued
	return relayAdjacentJSON("type", "data", "command_queued", plain(v))
}

// UnmarshalJSON decodes the interface-typed fields through their variant tables.
func (v *RelayObservationCommandQueued) UnmarshalJSON(data []byte) error {
	type plain RelayObservationCommandQueued
	aux := struct {
		*plain
		Command json.RawMessage `json:"command"`
	}{plain: (*plain)(v)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if len(aux.Command) > 0 && string(aux.Command) != "null" {
		decoded, err := UnmarshalRelayCommandJSON(aux.Command)
		if err != nil {
			return err
		}
		v.Command = decoded
	}
	return nil
}

type RelayObservationCommandStarted struct {
	CommandID   string `json:"command_id"`
	StartedAtMs int64  `json:"started_at_ms"`
}

func (RelayObservationCommandStarted) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationCommandStarted.
func (v RelayObservationCommandStarted) MarshalJSON() ([]byte, error) {
	type plain RelayObservationCommandStarted
	return relayAdjacentJSON("type", "data", "command_started", plain(v))
}

type RelayObservationCommandCompleted struct {
	BarrierCommandID *string             `json:"barrier_command_id,omitempty"`
	Command          *RelayCommandKind   `json:"command,omitempty"`
	CommandID        string              `json:"command_id"`
	Outcome          RelayCommandOutcome `json:"outcome"`
}

func (RelayObservationCommandCompleted) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationCommandCompleted.
func (v RelayObservationCommandCompleted) MarshalJSON() ([]byte, error) {
	type plain RelayObservationCommandCompleted
	return relayAdjacentJSON("type", "data", "command_completed", plain(v))
}

// UnmarshalJSON decodes the interface-typed fields through their variant tables.
func (v *RelayObservationCommandCompleted) UnmarshalJSON(data []byte) error {
	type plain RelayObservationCommandCompleted
	aux := struct {
		*plain
		Outcome json.RawMessage `json:"outcome"`
	}{plain: (*plain)(v)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	if len(aux.Outcome) > 0 && string(aux.Outcome) != "null" {
		decoded, err := UnmarshalRelayCommandOutcomeJSON(aux.Outcome)
		if err != nil {
			return err
		}
		v.Outcome = decoded
	}
	return nil
}

type RelayObservationRetryAssessmentStarted struct {
	CommandID string       `json:"command_id"`
	Evidence  TurnEvidence `json:"evidence"`
}

func (RelayObservationRetryAssessmentStarted) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationRetryAssessmentStarted.
func (v RelayObservationRetryAssessmentStarted) MarshalJSON() ([]byte, error) {
	type plain RelayObservationRetryAssessmentStarted
	return relayAdjacentJSON("type", "data", "retry_assessment_started", plain(v))
}

type RelayObservationRetryAssessmentResolved struct {
	CommandID         string `json:"command_id"`
	AssessmentOrdinal uint64 `json:"assessment_ordinal"`
	Retryable         bool   `json:"retryable"`
}

func (RelayObservationRetryAssessmentResolved) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationRetryAssessmentResolved.
func (v RelayObservationRetryAssessmentResolved) MarshalJSON() ([]byte, error) {
	type plain RelayObservationRetryAssessmentResolved
	return relayAdjacentJSON("type", "data", "retry_assessment_resolved", plain(v))
}

type RelayObservationCommandRejected struct {
	Reason    *OutcomeReason   `json:"reason,omitempty"`
	CommandID string           `json:"command_id"`
	Command   RelayCommandKind `json:"command"`
	Message   string           `json:"message"`
}

func (RelayObservationCommandRejected) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationCommandRejected.
func (v RelayObservationCommandRejected) MarshalJSON() ([]byte, error) {
	type plain RelayObservationCommandRejected
	return relayAdjacentJSON("type", "data", "command_rejected", plain(v))
}

type RelayObservationCommandInterrupted struct {
	// Atomically transfer the interrupted prompt's payload to recovery.
	ContinuationID *string          `json:"continuation_id,omitempty"`
	Reason         *OutcomeReason   `json:"reason,omitempty"`
	CommandID      string           `json:"command_id"`
	Command        RelayCommandKind `json:"command"`
	Message        string           `json:"message"`
}

func (RelayObservationCommandInterrupted) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationCommandInterrupted.
func (v RelayObservationCommandInterrupted) MarshalJSON() ([]byte, error) {
	type plain RelayObservationCommandInterrupted
	return relayAdjacentJSON("type", "data", "command_interrupted", plain(v))
}

type RelayObservationUserShellOutput struct {
	CommandID       string `json:"command_id"`
	Command         string `json:"command"`
	Stdout          string `json:"stdout"`
	Stderr          string `json:"stderr"`
	StdoutTruncated bool   `json:"stdout_truncated"`
	StderrTruncated bool   `json:"stderr_truncated"`
}

func (RelayObservationUserShellOutput) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationUserShellOutput.
func (v RelayObservationUserShellOutput) MarshalJSON() ([]byte, error) {
	type plain RelayObservationUserShellOutput
	return relayAdjacentJSON("type", "data", "user_shell_output", plain(v))
}

type RelayObservationConfigurationUpdated struct {
	Key   string `json:"key"`
	Value string `json:"value"`
}

func (RelayObservationConfigurationUpdated) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationConfigurationUpdated.
func (v RelayObservationConfigurationUpdated) MarshalJSON() ([]byte, error) {
	type plain RelayObservationConfigurationUpdated
	return relayAdjacentJSON("type", "data", "configuration_updated", plain(v))
}

type RelayObservationCheckpointReady struct {
	CommandID string `json:"command_id"`
	Through   uint64 `json:"through"`
}

func (RelayObservationCheckpointReady) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationCheckpointReady.
func (v RelayObservationCheckpointReady) MarshalJSON() ([]byte, error) {
	type plain RelayObservationCheckpointReady
	return relayAdjacentJSON("type", "data", "checkpoint_ready", plain(v))
}

type RelayObservationSessionFault struct {
	Reason  OutcomeReason `json:"reason"`
	Message string        `json:"message"`
}

func (RelayObservationSessionFault) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationSessionFault.
func (v RelayObservationSessionFault) MarshalJSON() ([]byte, error) {
	type plain RelayObservationSessionFault
	return relayAdjacentJSON("type", "data", "session_fault", plain(v))
}

type RelayObservationWarning struct {
	Message string `json:"message"`
}

func (RelayObservationWarning) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationWarning.
func (v RelayObservationWarning) MarshalJSON() ([]byte, error) {
	type plain RelayObservationWarning
	return relayAdjacentJSON("type", "data", "warning", plain(v))
}

// The target-side session control plane was replaced. This is a typed
// transcript event so clients can surface it as unread attention without
// interpreting arbitrary system text.
type RelayObservationSessionRestarted struct{}

func (RelayObservationSessionRestarted) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationSessionRestarted.
func (v RelayObservationSessionRestarted) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "session_restarted", nil)
}

// What a client-run terminal produced, journaled once when its child was
// reaped. The agent already read the full output over terminal/output;
// this copy is tail-capped for the person reading the transcript.
type RelayObservationTerminalOutput struct {
	TerminalID string  `json:"terminal_id"`
	Output     string  `json:"output"`
	Truncated  bool    `json:"truncated"`
	ExitCode   *uint32 `json:"exit_code"`
	Signal     *string `json:"signal"`
}

func (RelayObservationTerminalOutput) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationTerminalOutput.
func (v RelayObservationTerminalOutput) MarshalJSON() ([]byte, error) {
	type plain RelayObservationTerminalOutput
	return relayAdjacentJSON("type", "data", "terminal_output", plain(v))
}

// A controller-authored conversation line. Unlike a warning it reports
// something Hel did on purpose, so it reaches the transcript unadorned.
type RelayObservationNotice struct {
	Message string `json:"message"`
}

func (RelayObservationNotice) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationNotice.
func (v RelayObservationNotice) MarshalJSON() ([]byte, error) {
	type plain RelayObservationNotice
	return relayAdjacentJSON("type", "data", "notice", plain(v))
}

// A hook has temporarily claimed these events. They become delivered
// only when the hook acknowledges its flushed stdout response.
type RelayObservationMailboxHookLeaseCreated struct {
	Lease MailboxHookLease `json:"lease"`
}

func (RelayObservationMailboxHookLeaseCreated) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationMailboxHookLeaseCreated.
func (v RelayObservationMailboxHookLeaseCreated) MarshalJSON() ([]byte, error) {
	type plain RelayObservationMailboxHookLeaseCreated
	return relayAdjacentJSON("type", "data", "mailbox_hook_lease_created", plain(v))
}

// Return an unacknowledged hook lease to the head of the pending queue.
type RelayObservationMailboxHookLeaseReturned struct {
	LeaseID string                       `json:"lease_id"`
	Reason  MailboxHookLeaseReturnReason `json:"reason"`
}

func (RelayObservationMailboxHookLeaseReturned) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationMailboxHookLeaseReturned.
func (v RelayObservationMailboxHookLeaseReturned) MarshalJSON() ([]byte, error) {
	type plain RelayObservationMailboxHookLeaseReturned
	return relayAdjacentJSON("type", "data", "mailbox_hook_lease_returned", plain(v))
}

// Events were acknowledged by a tool hook or attached to one queued
// prompt. MailboxWake's CommandQueued event records the wake path itself.
type RelayObservationMailboxEventsDelivered struct {
	EventKeys       []string            `json:"event_keys"`
	Path            MailboxDeliveryPath `json:"path"`
	PromptCommandID *string             `json:"prompt_command_id,omitempty"`
	HookEvent       *string             `json:"hook_event,omitempty"`
	// Event content for a useful user-facing delivery notice. Empty on
	// older journal records, whose summaries remain count-only.
	Events []MailboxEvent `json:"events,omitempty"`
	// Present for hook delivery acknowledged against a durable lease.
	LeaseID *string `json:"lease_id,omitempty"`
	// A steer prompt delivered by the same acknowledged hook lease.
	Steering *MailboxHookSteering `json:"steering,omitempty"`
}

func (RelayObservationMailboxEventsDelivered) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationMailboxEventsDelivered.
func (v RelayObservationMailboxEventsDelivered) MarshalJSON() ([]byte, error) {
	type plain RelayObservationMailboxEventsDelivered
	return relayAdjacentJSON("type", "data", "mailbox_events_delivered", plain(v))
}

// The harness began working. Claude records this on an autonomous turn
// announcement or output without a prompt; Codex records native execution starts,
// including ordinary replies. Only starts outside a user turn add an
// autonomous-turn transcript marker.
type RelayObservationHarnessTurnStarted struct {
	StartedAtMs int64 `json:"started_at_ms"`
}

func (RelayObservationHarnessTurnStarted) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationHarnessTurnStarted.
func (v RelayObservationHarnessTurnStarted) MarshalJSON() ([]byte, error) {
	type plain RelayObservationHarnessTurnStarted
	return relayAdjacentJSON("type", "data", "harness_turn_started", plain(v))
}

// The harness reached a turn boundary on its own. Origin is the
// adapter's reported origin kind, kept for diagnostics.
type RelayObservationHarnessTurnSettled struct {
	// Present only for an autonomous turn; absent in older journals to preserve digests.
	Autonomous *SettledHarnessTurn `json:"autonomous,omitempty"`
	Origin     *string             `json:"origin"`
	// Whether a prompt of ours was still running when the turn settled.
	// The relay keeps Running for it; the projection cannot see
	// active_prompt, so the event carries the answer.
	PromptInFlight bool `json:"prompt_in_flight"`
}

func (RelayObservationHarnessTurnSettled) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationHarnessTurnSettled.
func (v RelayObservationHarnessTurnSettled) MarshalJSON() ([]byte, error) {
	type plain RelayObservationHarnessTurnSettled
	return relayAdjacentJSON("type", "data", "harness_turn_settled", plain(v))
}

type RelayObservationClosing struct{}

func (RelayObservationClosing) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationClosing.
func (v RelayObservationClosing) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "closing", nil)
}

type RelayObservationClosed struct{}

func (RelayObservationClosed) isRelayObservation() {}

// MarshalJSON writes the serde-tagged form of RelayObservationClosed.
func (v RelayObservationClosed) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "closed", nil)
}

// relayObservationVariants decodes each RelayObservation variant by its wire name.
var relayObservationVariants = map[string]func(json.RawMessage) (RelayObservation, error){
	"question_target_updated":     relayVariant[RelayObservationQuestionTargetUpdated, RelayObservation](false),
	"question_locator_finished":   relayVariant[RelayObservationQuestionLocatorFinished, RelayObservation](false),
	"managed_subagent_configured": relayVariant[RelayObservationManagedSubagentConfigured, RelayObservation](false),
	"work_liveness_changed":       relayVariant[RelayObservationWorkLivenessChanged, RelayObservation](false),
	"handback_check_started":      relayVariant[RelayObservationHandbackCheckStarted, RelayObservation](false),
	"turn_assessment_updated":     relayVariant[RelayObservationTurnAssessmentUpdated, RelayObservation](false),
	"steering_unconfirmed":        relayVariant[RelayObservationSteeringUnconfirmed, RelayObservation](false),
	"native_agent":                relayVariant[RelayObservationNativeAgent, RelayObservation](false),
	"agent_initialized":           relayVariant[RelayObservationAgentInitialized, RelayObservation](false),
	"session_opened":              relayVariant[RelayObservationSessionOpened, RelayObservation](false),
	"session_configured":          relayVariant[RelayObservationSessionConfigured, RelayObservation](false),
	"session_modes_configured":    relayVariant[RelayObservationSessionModesConfigured, RelayObservation](false),
	"session_update":              relayVariant[RelayObservationSessionUpdate, RelayObservation](false),
	"permission_auto_approved":    relayVariant[RelayObservationPermissionAutoApproved, RelayObservation](false),
	"elicitation_requested":       relayVariant[RelayObservationElicitationRequested, RelayObservation](false),
	"elicitation_resolved":        relayVariant[RelayObservationElicitationResolved, RelayObservation](false),
	"elicitations_cleared":        relayVariant[RelayObservationElicitationsCleared, RelayObservation](false),
	"command_queued":              relayVariant[RelayObservationCommandQueued, RelayObservation](false),
	"command_started":             relayVariant[RelayObservationCommandStarted, RelayObservation](false),
	"command_completed":           relayVariant[RelayObservationCommandCompleted, RelayObservation](false),
	"retry_assessment_started":    relayVariant[RelayObservationRetryAssessmentStarted, RelayObservation](false),
	"retry_assessment_resolved":   relayVariant[RelayObservationRetryAssessmentResolved, RelayObservation](false),
	"command_rejected":            relayVariant[RelayObservationCommandRejected, RelayObservation](false),
	"command_interrupted":         relayVariant[RelayObservationCommandInterrupted, RelayObservation](false),
	"user_shell_output":           relayVariant[RelayObservationUserShellOutput, RelayObservation](false),
	"configuration_updated":       relayVariant[RelayObservationConfigurationUpdated, RelayObservation](false),
	"checkpoint_ready":            relayVariant[RelayObservationCheckpointReady, RelayObservation](false),
	"session_fault":               relayVariant[RelayObservationSessionFault, RelayObservation](false),
	"warning":                     relayVariant[RelayObservationWarning, RelayObservation](false),
	"session_restarted":           relayVariant[RelayObservationSessionRestarted, RelayObservation](false),
	"terminal_output":             relayVariant[RelayObservationTerminalOutput, RelayObservation](false),
	"notice":                      relayVariant[RelayObservationNotice, RelayObservation](false),
	"mailbox_hook_lease_created":  relayVariant[RelayObservationMailboxHookLeaseCreated, RelayObservation](false),
	"mailbox_hook_lease_returned": relayVariant[RelayObservationMailboxHookLeaseReturned, RelayObservation](false),
	"mailbox_events_delivered":    relayVariant[RelayObservationMailboxEventsDelivered, RelayObservation](false),
	"harness_turn_started":        relayVariant[RelayObservationHarnessTurnStarted, RelayObservation](false),
	"harness_turn_settled":        relayVariant[RelayObservationHarnessTurnSettled, RelayObservation](false),
	"closing":                     relayVariant[RelayObservationClosing, RelayObservation](false),
	"closed":                      relayVariant[RelayObservationClosed, RelayObservation](false),
}

// UnmarshalRelayObservationJSON decodes the serde-tagged form of RelayObservation.
func UnmarshalRelayObservationJSON(data []byte) (RelayObservation, error) {
	name, body, err := relaySplitAdjacent(data, "type", "data", false)
	if err != nil {
		return nil, fmt.Errorf("decode RelayObservation: %w", err)
	}
	decode, ok := relayObservationVariants[name]
	if !ok {
		return nil, fmt.Errorf("decode RelayObservation: unknown variant %q", name)
	}
	return decode(body)
}

// RelayCommandOutcome is how a command reached its terminal state.
type RelayCommandOutcome interface {
	json.Marshaler
	isRelayCommandOutcome()
}

type RelayCommandOutcomeContextCleared struct {
	NativeSessionID string  `json:"native_session_id"`
	Memory          *string `json:"memory,omitempty"`
}

func (RelayCommandOutcomeContextCleared) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomeContextCleared.
func (v RelayCommandOutcomeContextCleared) MarshalJSON() ([]byte, error) {
	type plain RelayCommandOutcomeContextCleared
	return relayAdjacentJSON("type", "data", "context_cleared", plain(v))
}

type RelayCommandOutcomePrompt struct {
	StopReason string          `json:"stop_reason"`
	Diagnostic *TurnDiagnostic `json:"diagnostic,omitempty"`
	Usage      *TokenUsage     `json:"usage,omitempty"`
	// What the worker observed of a sub-agent's processes and working tree
	// when the turn ended, captured before this outcome is published, so
	// the facts and the outcome are one record and replay carries them.
	// Absent for a session that is not a sub-agent and from older workers.
	WorkFacts *WorkFacts `json:"work_facts,omitempty"`
}

func (RelayCommandOutcomePrompt) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomePrompt.
func (v RelayCommandOutcomePrompt) MarshalJSON() ([]byte, error) {
	type plain RelayCommandOutcomePrompt
	return relayAdjacentJSON("type", "data", "prompt", plain(v))
}

type RelayCommandOutcomeUserShell struct {
	Result UserShellResult `json:"result"`
}

func (RelayCommandOutcomeUserShell) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomeUserShell.
func (v RelayCommandOutcomeUserShell) MarshalJSON() ([]byte, error) {
	type plain RelayCommandOutcomeUserShell
	return relayAdjacentJSON("type", "data", "user_shell", plain(v))
}

type RelayCommandOutcomeUserShellCancelled struct{}

func (RelayCommandOutcomeUserShellCancelled) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomeUserShellCancelled.
func (v RelayCommandOutcomeUserShellCancelled) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "user_shell_cancelled", nil)
}

type RelayCommandOutcomeConfigured struct{}

func (RelayCommandOutcomeConfigured) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomeConfigured.
func (v RelayCommandOutcomeConfigured) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "configured", nil)
}

type RelayCommandOutcomeGoalControlled struct{}

func (RelayCommandOutcomeGoalControlled) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomeGoalControlled.
func (v RelayCommandOutcomeGoalControlled) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "goal_controlled", nil)
}

type RelayCommandOutcomeSessionModeSet struct{}

func (RelayCommandOutcomeSessionModeSet) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomeSessionModeSet.
func (v RelayCommandOutcomeSessionModeSet) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "session_mode_set", nil)
}

type RelayCommandOutcomeCancelled struct{}

func (RelayCommandOutcomeCancelled) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomeCancelled.
func (v RelayCommandOutcomeCancelled) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "cancelled", nil)
}

type RelayCommandOutcomeSteered struct {
	QueuedCommandID string `json:"queued_command_id"`
}

func (RelayCommandOutcomeSteered) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomeSteered.
func (v RelayCommandOutcomeSteered) MarshalJSON() ([]byte, error) {
	type plain RelayCommandOutcomeSteered
	return relayAdjacentJSON("type", "data", "steered", plain(v))
}

// The harness had no running turn to take the steered prompt and handed
// it back. The prompt keeps its place at the head of the queue.
type RelayCommandOutcomeSteeringReturned struct {
	QueuedCommandID string `json:"queued_command_id"`
}

func (RelayCommandOutcomeSteeringReturned) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomeSteeringReturned.
func (v RelayCommandOutcomeSteeringReturned) MarshalJSON() ([]byte, error) {
	type plain RelayCommandOutcomeSteeringReturned
	return relayAdjacentJSON("type", "data", "steering_returned", plain(v))
}

type RelayCommandOutcomeClosed struct{}

func (RelayCommandOutcomeClosed) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomeClosed.
func (v RelayCommandOutcomeClosed) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "closed", nil)
}

type RelayCommandOutcomeQueueChanged struct {
	RemovedCommandIds []string `json:"removed_command_ids"`
}

func (RelayCommandOutcomeQueueChanged) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomeQueueChanged.
func (v RelayCommandOutcomeQueueChanged) MarshalJSON() ([]byte, error) {
	type plain RelayCommandOutcomeQueueChanged
	return relayAdjacentJSON("type", "data", "queue_changed", plain(v))
}

type RelayCommandOutcomeCheckpointCompleted struct{}

func (RelayCommandOutcomeCheckpointCompleted) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomeCheckpointCompleted.
func (v RelayCommandOutcomeCheckpointCompleted) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "checkpoint_completed", nil)
}

type RelayCommandOutcomeCheckpointReleased struct{}

func (RelayCommandOutcomeCheckpointReleased) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomeCheckpointReleased.
func (v RelayCommandOutcomeCheckpointReleased) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "checkpoint_released", nil)
}

type RelayCommandOutcomeRecoveryFloorAdvanced struct{}

func (RelayCommandOutcomeRecoveryFloorAdvanced) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomeRecoveryFloorAdvanced.
func (v RelayCommandOutcomeRecoveryFloorAdvanced) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "recovery_floor_advanced", nil)
}

type RelayCommandOutcomeNoticeRecorded struct{}

func (RelayCommandOutcomeNoticeRecorded) isRelayCommandOutcome() {}

// MarshalJSON writes the serde-tagged form of RelayCommandOutcomeNoticeRecorded.
func (v RelayCommandOutcomeNoticeRecorded) MarshalJSON() ([]byte, error) {
	return relayAdjacentJSON("type", "data", "notice_recorded", nil)
}

// relayCommandOutcomeVariants decodes each RelayCommandOutcome variant by its wire name.
var relayCommandOutcomeVariants = map[string]func(json.RawMessage) (RelayCommandOutcome, error){
	"context_cleared":         relayVariant[RelayCommandOutcomeContextCleared, RelayCommandOutcome](false),
	"prompt":                  relayVariant[RelayCommandOutcomePrompt, RelayCommandOutcome](false),
	"user_shell":              relayVariant[RelayCommandOutcomeUserShell, RelayCommandOutcome](false),
	"user_shell_cancelled":    relayVariant[RelayCommandOutcomeUserShellCancelled, RelayCommandOutcome](false),
	"configured":              relayVariant[RelayCommandOutcomeConfigured, RelayCommandOutcome](false),
	"goal_controlled":         relayVariant[RelayCommandOutcomeGoalControlled, RelayCommandOutcome](false),
	"session_mode_set":        relayVariant[RelayCommandOutcomeSessionModeSet, RelayCommandOutcome](false),
	"cancelled":               relayVariant[RelayCommandOutcomeCancelled, RelayCommandOutcome](false),
	"steered":                 relayVariant[RelayCommandOutcomeSteered, RelayCommandOutcome](false),
	"steering_returned":       relayVariant[RelayCommandOutcomeSteeringReturned, RelayCommandOutcome](false),
	"closed":                  relayVariant[RelayCommandOutcomeClosed, RelayCommandOutcome](false),
	"queue_changed":           relayVariant[RelayCommandOutcomeQueueChanged, RelayCommandOutcome](false),
	"checkpoint_completed":    relayVariant[RelayCommandOutcomeCheckpointCompleted, RelayCommandOutcome](false),
	"checkpoint_released":     relayVariant[RelayCommandOutcomeCheckpointReleased, RelayCommandOutcome](false),
	"recovery_floor_advanced": relayVariant[RelayCommandOutcomeRecoveryFloorAdvanced, RelayCommandOutcome](false),
	"notice_recorded":         relayVariant[RelayCommandOutcomeNoticeRecorded, RelayCommandOutcome](false),
}

// UnmarshalRelayCommandOutcomeJSON decodes the serde-tagged form of RelayCommandOutcome.
func UnmarshalRelayCommandOutcomeJSON(data []byte) (RelayCommandOutcome, error) {
	name, body, err := relaySplitAdjacent(data, "type", "data", false)
	if err != nil {
		return nil, fmt.Errorf("decode RelayCommandOutcome: %w", err)
	}
	decode, ok := relayCommandOutcomeVariants[name]
	if !ok {
		return nil, fmt.Errorf("decode RelayCommandOutcome: unknown variant %q", name)
	}
	return decode(body)
}

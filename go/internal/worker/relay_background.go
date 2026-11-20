// Port of mj-worker/src/relay/background.rs.
//
// Agent-owned background work: adapter (AIR) tasks, Kimi's detached agents
// and the hosted terminals the agent left running.

package worker

import (
	"encoding/json"
	"fmt"
	"strings"
	"unicode"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func backgroundTaskID(kind, providerID string) string {
	return kind + ":" + providerID
}

// AirTaskEntry is one adapter task and the ACP session that owns its stop
// operation.
type AirTaskEntry struct {
	sessionID string
	taskID    string
	state     AirTaskState
}

// AirTaskStateKind names the variant an AirTaskState holds.
type AirTaskStateKind int

const (
	AirTaskStateKindBackground AirTaskStateKind = iota
	// AirTaskStateKindTurn retains ended turn IDs as tombstones until the
	// adapter restarts.
	AirTaskStateKindTurn
	// AirTaskStateKindHeldTurn: the adapter started after its prompt
	// response, which ACP has not relayed yet.
	AirTaskStateKindHeldTurn
	// AirTaskStateKindHeldTurnEnded: the task ended, but its preceding prompt
	// response is still in transit.
	AirTaskStateKindHeldTurnEnded
)

// AirTaskState is Rust's AirTaskState enum as a kind struct, so a turn's
// ordinal can be cleared in place.
type AirTaskState struct {
	Kind AirTaskStateKind
	// Background is the command of an AirTaskStateKindBackground task.
	Background core.BackgroundCommand
	// TurnOrdinal is the first ordinal of an AirTaskStateKindTurn task's
	// turn; nil once that turn ended.
	TurnOrdinal *uint64
}

func (e *AirTaskEntry) turnOrdinal() (uint64, bool) {
	if e.state.Kind != AirTaskStateKindTurn || e.state.TurnOrdinal == nil {
		return 0, false
	}
	return *e.state.TurnOrdinal, true
}

func (e *AirTaskEntry) command() *core.BackgroundCommand {
	if e.state.Kind != AirTaskStateKindBackground {
		return nil
	}
	return &e.state.Background
}

func airTaskKey(sessionID, taskID string) string {
	return fmt.Sprintf("%d:%s:%s", len(sessionID), sessionID, taskID)
}

// KimiTaskEntry is a Kimi native task together with the ACP launcher call it
// came from, so a hosted terminal bound to that call is not listed as a
// second job.
type KimiTaskEntry struct {
	command          core.BackgroundCommand
	parentToolCallID *string
}

type KimiProvisionalTask struct {
	command      core.BackgroundCommand
	nativeTaskID *string
}

// isAgentOutput is whether this update is agent output, which is what
// reveals that the harness is working. Everything else is session
// bookkeeping the harness also sends while idle.
func isAgentOutput(update *acpschema.SessionUpdate) bool {
	switch update.Kind {
	case acpschema.SessionUpdateKindAgentMessageChunk,
		acpschema.SessionUpdateKindAgentThoughtChunk,
		acpschema.SessionUpdateKindToolCall,
		acpschema.SessionUpdateKindToolCallUpdate,
		acpschema.SessionUpdateKindPlan:
		return true
	}
	return false
}

// claudeStopAcknowledgementPrefix is how the Claude adapter begins the plain
// agent_message_chunk it publishes when a user-requested stop ends a
// background task. The SDK injects nothing into the model for that stop, so
// no model cycle and no result follow it. Only the prefix is stable: the
// name that follows is whatever the adapter currently calls the task, which
// later AIR progress messages can rename. Checked against claude-agent-acp
// 0.81.0 dist/async-tasks.js (taskStopped, via noticeTranscriptText) on
// 2026-09-22.
const claudeStopAcknowledgementPrefix = "**Task stopped by user:** "

// isStopAcknowledgementText is whether text is the adapter's stop
// acknowledgement. The adapter renders the notice with transcriptBlock
// (dist/session-notices.js, claude-agent-acp 0.102.0 and later): a
// blockquote paragraph with blank lines on both sides,
// "\n\n> **Task stopped by user:** <name>.\n\n", so the marker is not at the
// start of the chunk. The bare form is what older adapters sent.
func isStopAcknowledgementText(text string) bool {
	text = strings.TrimLeftFunc(text, unicode.IsSpace)
	text = strings.TrimPrefix(text, "> ")
	return strings.HasPrefix(text, claudeStopAcknowledgementPrefix)
}

// agentChunkText is the text of an agent message chunk whose content is a
// single text block.
func agentChunkText(update *acpschema.SessionUpdate) (string, bool) {
	if update.Kind != acpschema.SessionUpdateKindAgentMessageChunk || update.AgentMessageChunk == nil {
		return "", false
	}
	content := update.AgentMessageChunk.Content
	if content.Kind != acpschema.ContentBlockKindText || content.Text == nil {
		return "", false
	}
	return content.Text.Text, true
}

func kimiNativeToolCallID(toolCallID string) string {
	if index := strings.LastIndexByte(toolCallID, ':'); index >= 0 {
		return toolCallID[index+1:]
	}
	return toolCallID
}

func kimiRunningTaskID(rawOutput json.RawMessage) (string, bool) {
	panic("unported: mj-worker/src/relay/background.rs kimi_running_task_id")
}

func kimiAgentDescription(title *string, rawInput json.RawMessage, taskID *string) string {
	panic("unported: mj-worker/src/relay/background.rs kimi_agent_description")
}

// SetBackgroundWorkPolicy chooses where commands the agent left running are
// read from.
func (r *DurableRelay) SetBackgroundWorkPolicy(policy BackgroundWorkPolicy) {
	r.backgroundWork = policy
	r.backgroundWorkKnown = nil
	if policy == BackgroundWorkPolicyKimiTasks {
		known := false
		r.backgroundWorkKnown = &known
	}
}

func (r *DurableRelay) nativeAgentCount() int {
	count := 0
	for _, agent := range r.snapshot.NativeAgents {
		if agent.State == core.NativeAgentStateRunning {
			count++
		}
	}
	return count
}

// backgroundCommands is the commands the agent left running with nothing
// waiting on them, oldest first.
//
// A hosted terminal only counts while nothing of ours is in flight: until
// then it is the turn's own work, not something left behind. AIR tasks count
// as soon as their adapter announces them.
//
// Kimi is the exception to the in-flight rule: it detaches a shell into one
// of our terminals and keeps working, so a terminal its native journal calls
// detached is listed mid-turn. That terminal then represents the job, and
// the Kimi task entry behind it is left out.
func (r *DurableRelay) backgroundCommands() []core.BackgroundCommand {
	panic("unported: mj-worker/src/relay/background.rs background_commands")
}

// boundAgentTerminal is the live hosted terminal an ACP launcher tool call
// embedded, if any.
func (r *DurableRelay) boundAgentTerminal(toolCallID *string) (string, bool) {
	if toolCallID == nil {
		return "", false
	}
	terminalID, ok := r.agentTerminalToolCalls[kimiNativeToolCallID(*toolCallID)]
	if !ok {
		return "", false
	}
	if _, live := r.activeAgentTerminals[terminalID]; !live {
		return "", false
	}
	return terminalID, true
}

// detachedAgentTerminals is the terminals the agent explicitly detached,
// which stay listed mid-turn. Only Kimi reports this; every other policy
// yields an empty set.
func (r *DurableRelay) detachedAgentTerminals() map[string]struct{} {
	panic("unported: mj-worker/src/relay/background.rs detached_agent_terminals")
}

// airUpdateSettlesTurn is whether update is the terminal edge of the task
// that owns the open harness turn, which ends that turn (see
// AirTaskChanged).
func (r *DurableRelay) airUpdateSettlesTurn(sessionID string, update core.AirTaskUpdate) bool {
	panic("unported: mj-worker/src/relay/background.rs air_update_settles_turn")
}

// AirTaskChanged applies AIR lifecycle changes at the single owner of live
// background work.
func (r *DurableRelay) AirTaskChanged(sessionID string, update core.AirTaskUpdate) error {
	panic("unported: mj-worker/src/relay/background.rs air_task_changed")
}

func (r *DurableRelay) hasHeldClaudeTurn() bool {
	for _, entry := range r.airTasks {
		if entry.state.Kind == AirTaskStateKindHeldTurn || entry.state.Kind == AirTaskStateKindHeldTurnEnded {
			return true
		}
	}
	return false
}

func (r *DurableRelay) promoteHeldClaudeTurn() error {
	panic("unported: mj-worker/src/relay/background.rs promote_held_claude_turn")
}

func (r *DurableRelay) KimiNativeAgentsChanged(agents map[string]KimiNativeAgentEntry) error {
	panic("unported: mj-worker/src/relay/background.rs kimi_native_agents_changed")
}

func (r *DurableRelay) KimiBackgroundTasksChanged(tasks []KimiBackgroundTask, observedToolCallIDs, observedTaskIDs map[string]struct{}) error {
	panic("unported: mj-worker/src/relay/background.rs kimi_background_tasks_changed")
}

func (r *DurableRelay) KimiBackgroundTasksUnavailable() error {
	if r.backgroundWork == BackgroundWorkPolicyKimiTasks {
		known := false
		r.backgroundWorkKnown = &known
		return r.persistActivityTransition()
	}
	return nil
}

// noteTaskSettled records when a Claude background task completed or
// failed, while no turn is open to answer it.
func (r *DurableRelay) noteTaskSettled() {
	if r.snapshot.ActivePrompt == nil && r.snapshot.HarnessTurn == nil {
		now := core.EpochMillis()
		r.taskSettledAtMs = &now
	}
}

func (r *DurableRelay) BackgroundTaskStopTarget(requestedID string) (core.BackgroundTaskStopTarget, error) {
	panic("unported: mj-worker/src/relay/background.rs background_task_stop_target")
}

// ClaudeStopNotSent forgets a stop this process requested but could not
// send.
func (r *DurableRelay) ClaudeStopNotSent(sessionID, taskID string) {
	pending, ok := r.claudePendingStops[sessionID]
	if !ok {
		return
	}
	delete(pending, airTaskKey(sessionID, taskID))
	if len(pending) == 0 {
		delete(r.claudePendingStops, sessionID)
	}
}

func (r *DurableRelay) isClaudeStopAcknowledgementFor(sessionID *string, update *acpschema.SessionUpdate) bool {
	if r.harnessTurns != HarnessTurnPolicyClaudeAdapter || sessionID == nil || len(r.claudePendingStops[*sessionID]) == 0 {
		return false
	}
	text, ok := agentChunkText(update)
	return ok && isStopAcknowledgementText(text)
}

func (r *DurableRelay) isClaudeStopAcknowledgement(update *acpschema.SessionUpdate) bool {
	return r.isClaudeStopAcknowledgementFor(r.snapshot.NativeSessionID, update)
}

func (r *DurableRelay) consumeClaudeStopAcknowledgement(sessionID *string, update *acpschema.SessionUpdate) bool {
	panic("unported: mj-worker/src/relay/background.rs consume_claude_stop_acknowledgement")
}

func (r *DurableRelay) AgentTerminalStarted(terminal core.ActiveAgentTerminal) error {
	_, err := r.registerAgentTerminal(terminal)
	return err
}

func (r *DurableRelay) registerAgentTerminal(terminal core.ActiveAgentTerminal) (bool, error) {
	if _, closed := r.closedAgentTerminals[terminal.TerminalID]; closed {
		return false, nil
	}
	r.activeAgentTerminals[terminal.TerminalID] = terminal
	if err := r.persistActivityTransition(); err != nil {
		return false, err
	}
	return true, nil
}

// admitAgentTerminal registers a terminal the harness cannot announce
// itself and publishes the fallback tool call that shows it.
func (r *DurableRelay) admitAgentTerminal(terminal core.ActiveAgentTerminal) error {
	registered, err := r.registerAgentTerminal(terminal)
	if err != nil || !registered {
		return err
	}
	call := core.FallbackTerminalToolCall(terminal.TerminalID, terminal.Command)
	_, err = r.RecordSessionUpdate(acpschema.SessionUpdate{Kind: acpschema.SessionUpdateKindToolCall, ToolCall: &call})
	return err
}

func (r *DurableRelay) agentTerminalStartFailed(id string) error {
	panic("unported: mj-worker/src/relay/background.rs agent_terminal_start_failed")
}

func (r *DurableRelay) AgentTerminalClosed(terminalID string) error {
	delete(r.activeAgentTerminals, terminalID)
	r.foregroundTools.Close(core.FallbackTerminalToolCallID(terminalID))
	r.closedAgentTerminals[terminalID] = struct{}{}
	return r.persistActivityTransition()
}

// retireAgentTerminalConnection forgets the terminals of a torn-down
// connection. IDs already carry the registry's connection identity. An old
// FIFO retirement must preserve a replacement's synchronous reservations.
func (r *DurableRelay) retireAgentTerminalConnection(connectionID string) error {
	panic("unported: mj-worker/src/relay/background.rs retire_agent_terminal_connection")
}

func (r *DurableRelay) retireHarnessProcessState() error {
	r.forgetHarnessProcesses()
	return r.persistActivityTransition()
}

// forgetHarnessProcesses: a restart or a close ends the harness process that
// owned whatever it had left running, so nothing it reported is still alive.
func (r *DurableRelay) forgetHarnessProcesses() {
	r.pendingNativeResumes = nil
	clear(r.airTasks)
	clear(r.claudePendingStops)
	r.taskSettledAtMs = nil
	clear(r.kimiBackgroundTasks)
	clear(r.kimiProvisionalTasks)
	clear(r.kimiObservedTaskIDs)
	clear(r.kimiObservedToolIDs)
	clear(r.agentTerminalToolCalls)
	r.foregroundTools.Clear()
	if r.backgroundWork == BackgroundWorkPolicyKimiTasks {
		known := false
		r.backgroundWorkKnown = &known
	}
}

func (r *DurableRelay) trackAgentTerminalToolCall(update *acpschema.SessionUpdate) {
	panic("unported: mj-worker/src/relay/background.rs track_agent_terminal_tool_call")
}

func (r *DurableRelay) trackKimiBackgroundAgent(update *acpschema.SessionUpdate) {
	panic("unported: mj-worker/src/relay/background.rs track_kimi_background_agent")
}

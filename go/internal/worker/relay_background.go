// Port of mj-worker/src/relay/background.rs.
//
// Agent-owned background work: adapter (AIR) tasks, Kimi's detached agents,
// Antigravity's native tasks and the hosted terminals the agent left
// running.

package worker

import (
	"cmp"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"math"
	"slices"
	"strings"
	"unicode"

	acpschema "github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// antigravityWorkState is what this worker knows about Antigravity's
// native task state, from its hooks and its databases.
type antigravityWorkState struct {
	hookEpoch                  uint64
	acpEpochActive             bool
	staleStepIndices           map[int64]struct{}
	staleStepIndicesSet        bool
	summaryCommitGen           uint64
	summaryStatus              *AntigravityRunStatus
	summaryNotFullyIdle        bool
	summaryHasRunningSubagents bool
	summaryBusyStartedAtMs     *int64
	lastSummaryRowDigest       *[32]byte
	lastPreInvocationSteps     *int64
	pendingStopWork            *antigravityPendingStopWork
	hookIdleGen                *uint64
	warnedMissingFullyIdle     bool
	harnessTurnSawRunning      bool
	pendingTurnSettle          *string
	runningTasks               []core.BackgroundCommand
	taskFirstSeenMs            map[int64]int64
}

type antigravityPendingStopWork struct {
	startedAtMs int64
	// baselineDigest is the row already on disk at Stop time, if any.
	baselineDigest *[32]byte
	// minStepCount: NO_TOOL_CALL appends a step after PreInvocation's
	// initial step count.
	minStepCount *int64
	sawBusy      bool
}

func (w *antigravityWorkState) appendCommands(commands []core.BackgroundCommand, turnInFlight bool) []core.BackgroundCommand {
	commands = append(commands, w.runningTasks...)
	if len(w.runningTasks) != 0 || !w.acpEpochActive {
		return commands
	}
	summaryFresh := w.hookIdleGen == nil || *w.hookIdleGen != w.summaryCommitGen
	summaryRunning := w.summaryStatus != nil && w.summaryStatus.IsActive()
	summaryReportsBackground := summaryFresh &&
		(((w.summaryNotFullyIdle || summaryRunning) && !turnInFlight) || w.summaryHasRunningSubagents)
	if w.pendingStopWork != nil || summaryReportsBackground {
		id, command := "pending", "Antigravity background task"
		if w.summaryHasRunningSubagents {
			id, command = "subagent", "Antigravity subagent running"
		} else if summaryFresh && summaryRunning && !turnInFlight {
			id, command = "turn", "Antigravity turn running"
		}
		var startedAtMs int64
		switch {
		case w.pendingStopWork != nil:
			startedAtMs = w.pendingStopWork.startedAtMs
		case w.summaryBusyStartedAtMs != nil:
			startedAtMs = *w.summaryBusyStartedAtMs
		default:
			panic("background evidence has a start time")
		}
		commands = append(commands, core.BackgroundCommandAntigravity(id, startedAtMs, command))
	}
	return commands
}

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
	if len(rawOutput) == 0 {
		return "", false
	}
	var value any
	if json.Unmarshal(rawOutput, &value) != nil {
		return "", false
	}
	output, ok := value.(string)
	if !ok {
		object, isObject := value.(map[string]any)
		if !isObject {
			return "", false
		}
		if output, ok = object["output"].(string); !ok {
			return "", false
		}
	}
	lines := relayRustLines(output)
	if !slices.ContainsFunc(lines, func(line string) bool { return strings.TrimSpace(line) == "status: running" }) {
		return "", false
	}
	for _, line := range lines {
		if taskID, ok := strings.CutPrefix(strings.TrimSpace(line), "task_id:"); ok {
			if taskID = strings.TrimSpace(taskID); taskID != "" {
				return taskID, true
			}
		}
	}
	return "", false
}

// relayRustLines splits text as Rust's str::lines does: on "\n", dropping one
// trailing "\r" from each line and the empty piece after a final newline.
func relayRustLines(text string) []string {
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

func kimiAgentDescription(title *string, rawInput json.RawMessage, taskID *string) string {
	if len(rawInput) > 0 {
		var input map[string]any
		if json.Unmarshal(rawInput, &input) == nil {
			if description, ok := input["description"].(string); ok && strings.TrimSpace(description) != "" {
				return description
			}
		}
	}
	if title != nil {
		return *title
	}
	if taskID != nil {
		return *taskID
	}
	return "Kimi background agent"
}

// SetBackgroundWorkPolicy chooses where commands the agent left running are
// read from.
func (r *DurableRelay) SetBackgroundWorkPolicy(policy BackgroundWorkPolicy) {
	r.backgroundWork = policy
	r.backgroundWorkKnown = nil
	if policy == BackgroundWorkPolicyKimiTasks || policy == BackgroundWorkPolicyAntigravityTasks {
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
	turnInFlight := r.turnInProgress()
	detachedTerminals := r.detachedAgentTerminals()
	commands := []core.BackgroundCommand{}
	for _, id := range slices.Sorted(maps.Keys(r.activeAgentTerminals)) {
		terminal := r.activeAgentTerminals[id]
		if _, detached := detachedTerminals[terminal.TerminalID]; turnInFlight && !detached {
			continue
		}
		commands = append(commands, core.BackgroundCommand{
			ID:          backgroundTaskID("terminal", terminal.TerminalID),
			StartedAtMs: terminal.StartedAtMs,
			Command:     terminal.Command,
			CanStop:     true,
		})
	}
	if r.backgroundWork == BackgroundWorkPolicyAirTasks {
		for _, key := range slices.Sorted(maps.Keys(r.airTasks)) {
			if command := r.airTasks[key].command(); command != nil {
				commands = append(commands, *command)
			}
		}
	}
	if r.backgroundWork == BackgroundWorkPolicyKimiTasks {
		for _, id := range slices.Sorted(maps.Keys(r.kimiBackgroundTasks)) {
			entry := r.kimiBackgroundTasks[id]
			if _, bound := r.boundAgentTerminal(entry.parentToolCallID); !bound {
				commands = append(commands, entry.command)
			}
		}
		for _, toolCallID := range slices.Sorted(maps.Keys(r.kimiProvisionalTasks)) {
			id := toolCallID
			if _, bound := r.boundAgentTerminal(&id); !bound {
				commands = append(commands, r.kimiProvisionalTasks[toolCallID].command)
			}
		}
	}
	if r.backgroundWork == BackgroundWorkPolicyAntigravityTasks {
		commands = r.antigravityWork.appendCommands(commands, turnInFlight)
	}
	slices.SortStableFunc(commands, func(left, right core.BackgroundCommand) int {
		if order := cmp.Compare(left.StartedAtMs, right.StartedAtMs); order != 0 {
			return order
		}
		return strings.Compare(left.Command, right.Command)
	})
	return commands
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
	detached := map[string]struct{}{}
	if r.backgroundWork != BackgroundWorkPolicyKimiTasks {
		return detached
	}
	for _, entry := range r.kimiBackgroundTasks {
		if terminal, bound := r.boundAgentTerminal(entry.parentToolCallID); bound {
			detached[terminal] = struct{}{}
		}
	}
	for toolCallID := range r.kimiProvisionalTasks {
		id := toolCallID
		if terminal, bound := r.boundAgentTerminal(&id); bound {
			detached[terminal] = struct{}{}
		}
	}
	return detached
}

// airUpdateSettlesTurn is whether update is the terminal edge of the task
// that owns the open harness turn, which ends that turn (see
// AirTaskChanged).
func (r *DurableRelay) airUpdateSettlesTurn(sessionID string, update core.AirTaskUpdate) bool {
	finished, ok := update.(core.AirTaskUpdateFinished)
	if !ok {
		return false
	}
	if r.backgroundWork != BackgroundWorkPolicyAirTasks ||
		r.snapshot.Execution == core.RelayExecutionStateClosing ||
		r.snapshot.Execution == core.RelayExecutionStateClosed {
		return false
	}
	entry, ok := r.airTasks[airTaskKey(sessionID, finished.TaskID)]
	if !ok {
		return false
	}
	ordinal, ok := entry.turnOrdinal()
	return ok && r.snapshot.ActivePrompt == nil &&
		r.snapshot.HarnessTurn != nil && r.snapshot.HarnessTurn.FirstOrdinal == ordinal
}

// AirTaskChanged applies AIR lifecycle changes at the single owner of live
// background work.
func (r *DurableRelay) AirTaskChanged(sessionID string, update core.AirTaskUpdate) error {
	if r.backgroundWork != BackgroundWorkPolicyAirTasks ||
		r.snapshot.Execution == core.RelayExecutionStateClosing ||
		r.snapshot.Execution == core.RelayExecutionStateClosed {
		return nil
	}
	var taskID string
	switch update := update.(type) {
	case core.AirTaskUpdateTurnSpawned:
		taskID = update.TaskID
	case core.AirTaskUpdateSpawned:
		taskID = update.TaskID
	case core.AirTaskUpdateChanged:
		taskID = update.TaskID
	case core.AirTaskUpdateFinished:
		taskID = update.TaskID
	}
	key := airTaskKey(sessionID, taskID)
	if entry, ok := r.airTasks[key]; ok &&
		(entry.state.Kind == AirTaskStateKindTurn ||
			entry.state.Kind == AirTaskStateKindHeldTurn ||
			entry.state.Kind == AirTaskStateKindHeldTurnEnded) {
		if finished, ok := update.(core.AirTaskUpdateFinished); ok {
			var ordinal *uint64
			if entry.state.Kind == AirTaskStateKindTurn {
				ordinal = entry.state.TurnOrdinal
				entry.state.TurnOrdinal = nil
			} else {
				entry.state = AirTaskState{Kind: AirTaskStateKindHeldTurnEnded}
			}
			// The pinned bridge accounts for trailing idles (#1455). The task
			// identity owns completion, including after output.
			if ordinal != nil && r.snapshot.ActivePrompt == nil &&
				r.snapshot.HarnessTurn != nil && r.snapshot.HarnessTurn.FirstOrdinal == *ordinal {
				stopReason := "cancelled"
				switch {
				case finished.Failed:
					stopReason = promptErrorStopReason
				case finished.Settled:
					stopReason = "end_turn"
				}
				origin := "autonomous_task_ended"
				if err := r.settleHarnessTurn(&origin, stopReason); err != nil {
					return err
				}
			}
		}
		return r.persistActivityTransition()
	}
	switch update := update.(type) {
	case core.AirTaskUpdateTurnSpawned:
		liveRoot := !update.Replayed &&
			r.harnessTurns == HarnessTurnPolicyClaudeAdapter &&
			r.snapshot.NativeSessionID != nil && *r.snapshot.NativeSessionID == sessionID
		var state AirTaskState
		if liveRoot && r.snapshot.ActivePrompt != nil {
			state = AirTaskState{Kind: AirTaskStateKindHeldTurn}
		} else {
			if liveRoot {
				if err := r.openHarnessTurnIfAllowed(); err != nil {
					return err
				}
			}
			state = AirTaskState{Kind: AirTaskStateKindTurn}
			if liveRoot && r.snapshot.HarnessTurn != nil {
				first := r.snapshot.HarnessTurn.FirstOrdinal
				state.TurnOrdinal = &first
			}
		}
		r.airTasks[key] = &AirTaskEntry{sessionID: sessionID, taskID: update.TaskID, state: state}
	case core.AirTaskUpdateSpawned:
		startedAtMs := core.EpochMillis()
		if entry, ok := r.airTasks[key]; ok {
			if command := entry.command(); command != nil {
				startedAtMs = command.StartedAtMs
			}
		}
		r.airTasks[key] = &AirTaskEntry{
			sessionID: sessionID,
			taskID:    update.TaskID,
			state: AirTaskState{
				Kind: AirTaskStateKindBackground,
				Background: core.BackgroundCommand{
					ID:          backgroundTaskID("air", key),
					StartedAtMs: startedAtMs,
					Command:     update.Description,
					CanStop:     update.CanStop,
				},
			},
		}
	case core.AirTaskUpdateChanged:
		if entry, ok := r.airTasks[key]; ok && entry.state.Kind == AirTaskStateKindBackground {
			if update.Description != nil {
				entry.state.Background.Command = *update.Description
			}
			if update.CanStop != nil {
				entry.state.Background.CanStop = *update.CanStop
			}
		}
	case core.AirTaskUpdateFinished:
		_, removed := r.airTasks[key]
		delete(r.airTasks, key)
		if removed && update.Settled &&
			r.harnessTurns == HarnessTurnPolicyClaudeAdapter &&
			r.snapshot.NativeAgentReplay == nil {
			r.noteTaskSettled()
		}
	}
	return r.persistActivityTransition()
}

func (r *DurableRelay) hasHeldClaudeTurn() bool {
	for _, entry := range r.airTasks {
		if entry.state.Kind == AirTaskStateKindHeldTurn || entry.state.Kind == AirTaskStateKindHeldTurnEnded {
			return true
		}
	}
	return false
}

// promoteHeldClaudeTurn restores adapter order before queue promotion, under
// the relay's owner lock.
func (r *DurableRelay) promoteHeldClaudeTurn() error {
	if r.snapshot.ActivePrompt != nil {
		return nil
	}
	for _, entry := range r.airTasks {
		if entry.state.Kind == AirTaskStateKindHeldTurnEnded {
			entry.state = AirTaskState{Kind: AirTaskStateKindTurn}
		}
	}
	keys := slices.Sorted(maps.Keys(r.airTasks))
	index := slices.IndexFunc(keys, func(key string) bool {
		return r.airTasks[key].state.Kind == AirTaskStateKindHeldTurn
	})
	if index < 0 {
		return nil
	}
	key := keys[index]
	if err := r.openHarnessTurnIfAllowed(); err != nil {
		return err
	}
	if turn := r.snapshot.HarnessTurn; turn != nil {
		entry, ok := r.airTasks[key]
		if !ok {
			panic("held task exists")
		}
		first := turn.FirstOrdinal
		entry.state = AirTaskState{Kind: AirTaskStateKindTurn, TurnOrdinal: &first}
	}
	return nil
}

// KimiNativeAgentsChanged replaces Kimi's native task level and reconciles
// ACP launch or query evidence against task and launcher identities
// retained by the wire.
func (r *DurableRelay) KimiNativeAgentsChanged(agents map[string]KimiNativeAgentEntry) error {
	var missing []string
	for _, id := range slices.Sorted(maps.Keys(r.snapshot.NativeAgents)) {
		if _, listed := agents[id]; !listed && r.snapshot.NativeAgents[id].State == core.NativeAgentStateRunning {
			missing = append(missing, id)
		}
	}
	for _, sessionID := range missing {
		if _, err := r.RecordObservation(core.RelayObservationNativeAgent{Event: core.NativeAgentEventState{
			SessionID: sessionID,
			State:     core.NativeAgentStateDisconnected,
		}}); err != nil {
			return err
		}
	}
	for _, id := range slices.Sorted(maps.Keys(agents)) {
		entry := agents[id]
		if _, known := r.snapshot.NativeAgents[id]; !known {
			if _, err := r.RecordObservation(core.RelayObservationNativeAgent{Event: core.NativeAgentEventSpawned{
				SessionID: id,
				Name:      entry.Task.Description,
				Task:      entry.Task.Description,
			}}); err != nil {
				return err
			}
		}
		if agent, known := r.snapshot.NativeAgents[id]; known && agent.State != entry.State {
			if _, err := r.RecordObservation(core.RelayObservationNativeAgent{Event: core.NativeAgentEventState{
				SessionID: id,
				State:     entry.State,
			}}); err != nil {
				return err
			}
		}
	}
	return nil
}

func (r *DurableRelay) KimiBackgroundTasksChanged(tasks []KimiBackgroundTask, observedToolCallIDs, observedTaskIDs map[string]struct{}) error {
	if r.backgroundWork != BackgroundWorkPolicyKimiTasks ||
		r.snapshot.Execution == core.RelayExecutionStateClosing ||
		r.snapshot.Execution == core.RelayExecutionStateClosed {
		return nil
	}
	if observedTaskIDs == nil {
		observedTaskIDs = map[string]struct{}{}
	}
	if observedToolCallIDs == nil {
		observedToolCallIDs = map[string]struct{}{}
	}
	r.kimiObservedTaskIDs = observedTaskIDs
	r.kimiObservedToolIDs = observedToolCallIDs
	for toolCallID, task := range r.kimiProvisionalTasks {
		_, observedTool := r.kimiObservedToolIDs[kimiNativeToolCallID(toolCallID)]
		observedTask := false
		if task.nativeTaskID != nil {
			_, observedTask = r.kimiObservedTaskIDs[*task.nativeTaskID]
		}
		if observedTool || observedTask {
			delete(r.kimiProvisionalTasks, toolCallID)
		}
	}
	confirmed := map[string]KimiTaskEntry{}
	for _, task := range tasks {
		if task.IsAgent {
			continue
		}
		var parent *string
		if task.ParentToolCallID != nil {
			id := *task.ParentToolCallID
			parent = &id
		}
		confirmed[task.TaskID] = KimiTaskEntry{
			command: core.BackgroundCommand{
				ID:          backgroundTaskID("kimi", task.TaskID),
				StartedAtMs: task.StartedAtMs,
				Command:     task.Description,
				CanStop:     false,
			},
			parentToolCallID: parent,
		}
	}
	r.kimiBackgroundTasks = confirmed
	known := true
	r.backgroundWorkKnown = &known
	return r.persistActivityTransition()
}

func (r *DurableRelay) KimiBackgroundTasksUnavailable() error {
	if r.backgroundWork == BackgroundWorkPolicyKimiTasks {
		known := false
		r.backgroundWorkKnown = &known
		return r.persistActivityTransition()
	}
	return nil
}

func (r *DurableRelay) forgetAntigravityState() {
	r.antigravityWork = antigravityWorkState{hookEpoch: r.antigravityWork.hookEpoch + 1}
	if r.backgroundWork == BackgroundWorkPolicyAntigravityTasks {
		known := false
		r.backgroundWorkKnown = &known
	}
}

// resetAntigravityState forgets every Antigravity observation, as a harness
// restart does, and publishes the change.
func (r *DurableRelay) resetAntigravityState() error {
	r.forgetAntigravityState()
	return r.persistActivityTransition()
}

func (r *DurableRelay) matchesAntigravityConversation(conversationID *string) bool {
	return conversationID != nil && *conversationID != "" &&
		r.snapshot.NativeSessionID != nil && *r.snapshot.NativeSessionID == *conversationID
}

func (r *DurableRelay) antigravityPromptStarted() {
	r.antigravityWork.hookEpoch++
	if r.harnessTurns == HarnessTurnPolicyAntigravity {
		wasActive := r.antigravityWork.acpEpochActive
		r.antigravityWork.acpEpochActive = true
		if !wasActive {
			known := false
			r.backgroundWorkKnown = &known
		}
	}
}

func (r *DurableRelay) antigravityPreInvocation(conversationID *string, initialNumSteps *int64) error {
	if !r.matchesAntigravityConversation(conversationID) {
		return nil
	}
	wasActive := r.antigravityWork.acpEpochActive
	r.antigravityWork.acpEpochActive = true
	r.antigravityWork.hookEpoch++
	if !wasActive {
		known := false
		r.backgroundWorkKnown = &known
	}
	r.antigravityWork.hookIdleGen = nil
	if initialNumSteps != nil {
		steps := *initialNumSteps
		r.antigravityWork.lastPreInvocationSteps = &steps
	}
	if r.canOpenHarnessTurn() {
		r.antigravityWork.harnessTurnSawRunning = false
		r.antigravityWork.pendingTurnSettle = nil
		if err := r.openHarnessTurnIfAllowed(); err != nil {
			return err
		}
	}
	return r.persistActivityTransition()
}

// antigravityStop applies an Antigravity Stop hook. observedBaselineDigest
// is Rust's Option<Option<[u8; 32]>>: nil when the summary row was not
// sampled, otherwise the sampled row's digest, itself nil when no row was
// on disk.
func (r *DurableRelay) antigravityStop(conversationID *string, fullyIdle *bool, terminationReason *string, observedBaselineDigest **[32]byte) error {
	if (r.backgroundWork != BackgroundWorkPolicyAntigravityTasks && r.harnessTurns != HarnessTurnPolicyAntigravity) ||
		!r.matchesAntigravityConversation(conversationID) {
		return nil
	}

	work := &r.antigravityWork
	work.hookEpoch++
	wasActive := work.acpEpochActive
	work.acpEpochActive = true
	if !wasActive {
		known := false
		r.backgroundWorkKnown = &known
	}
	if fullyIdle != nil && *fullyIdle {
		work.pendingStopWork = nil
		generation := work.summaryCommitGen
		work.hookIdleGen = &generation
	} else {
		var minStepCount *int64
		if terminationReason != nil && *terminationReason == "NO_TOOL_CALL" && work.lastPreInvocationSteps != nil {
			steps := *work.lastPreInvocationSteps
			if steps < math.MaxInt64 {
				steps++
			}
			minStepCount = &steps
		}
		startedAtMs := core.EpochMillis()
		if work.pendingStopWork != nil {
			startedAtMs = work.pendingStopWork.startedAtMs
		}
		baselineDigest := work.lastSummaryRowDigest
		if observedBaselineDigest != nil {
			baselineDigest = *observedBaselineDigest
		}
		work.pendingStopWork = &antigravityPendingStopWork{
			startedAtMs:    startedAtMs,
			baselineDigest: baselineDigest,
			minStepCount:   minStepCount,
		}
		work.hookIdleGen = nil
		if fullyIdle == nil && !work.warnedMissingFullyIdle {
			slog.Warn("Antigravity Stop hook omitted fullyIdle; treating background work as pending",
				"session_id", r.snapshot.SessionID)
			work.warnedMissingFullyIdle = true
		}
	}

	// This must publish under the relay lock before the hook is acknowledged
	// and ACP is free to finish the prompt.
	if err := r.noteBackgroundWork(); err != nil {
		return err
	}
	if err := r.persistActivityTransition(); err != nil {
		return err
	}
	if r.snapshot.ActivePrompt == nil && r.snapshot.AutonomousHarnessTurn() != nil {
		reason := "end_turn"
		if terminationReason != nil {
			switch *terminationReason {
			case "USER_CANCELED":
				reason = "cancelled"
			case "ERROR":
				reason = "error"
			}
		}
		r.antigravityWork.pendingTurnSettle = &reason
	}
	return nil
}

func (r *DurableRelay) AntigravityHookEpoch() uint64 {
	return r.antigravityWork.hookEpoch
}

// AntigravitySnapshotChanged applies one scan of Antigravity's databases. It
// reports false when a hook arrived after the scan began, so the scan's
// cursor must not be committed.
func (r *DurableRelay) AntigravitySnapshotChanged(snapshot AntigravitySnapshot, scanHookEpoch uint64) (bool, error) {
	if r.backgroundWork != BackgroundWorkPolicyAntigravityTasks {
		return true, nil
	}
	work := &r.antigravityWork
	if scanHookEpoch != work.hookEpoch {
		return false, nil
	}
	if snapshot.SummaryCommitChanged {
		work.summaryCommitGen++
	}
	work.lastSummaryRowDigest = snapshot.SummaryRowDigest
	indices := map[int64]struct{}{}
	for _, task := range snapshot.RunningTasks {
		indices[task.StepIndex] = struct{}{}
	}
	if !work.acpEpochActive {
		known := true
		r.backgroundWorkKnown = &known
		if !work.staleStepIndicesSet {
			work.staleStepIndices = indices
			work.staleStepIndicesSet = true
		}
		return true, r.persistActivityTransition()
	}
	known := snapshot.Summary != nil && snapshot.ConversationDBPresent
	r.backgroundWorkKnown = &known

	if work.staleStepIndicesSet {
		maps.DeleteFunc(work.staleStepIndices, func(index int64, _ struct{}) bool {
			_, live := indices[index]
			return !live
		})
	}
	maps.DeleteFunc(work.taskFirstSeenMs, func(index int64, _ int64) bool {
		_, live := indices[index]
		return !live
	})
	work.runningTasks = nil
	for _, task := range snapshot.RunningTasks {
		if _, stale := work.staleStepIndices[task.StepIndex]; work.staleStepIndicesSet && stale {
			continue
		}
		if work.taskFirstSeenMs == nil {
			work.taskFirstSeenMs = map[int64]int64{}
		}
		firstSeen, seen := work.taskFirstSeenMs[task.StepIndex]
		if !seen {
			firstSeen = core.EpochMillis()
			work.taskFirstSeenMs[task.StepIndex] = firstSeen
		}
		startedAtMs := firstSeen
		if task.StartedAtMs != nil {
			startedAtMs = *task.StartedAtMs
		}
		work.runningTasks = append(work.runningTasks, core.BackgroundCommandAntigravity(task.ID, startedAtMs, task.Command))
	}

	if summary := snapshot.Summary; summary != nil {
		status := summary.Status
		work.summaryStatus = &status
		work.summaryNotFullyIdle = summary.NotFullyIdle
		work.summaryHasRunningSubagents = summary.HasRunningSubagents
		if summary.NotFullyIdle || summary.HasRunningSubagents || summary.Status.IsActive() {
			if work.summaryBusyStartedAtMs == nil {
				now := core.EpochMillis()
				work.summaryBusyStartedAtMs = &now
			}
		} else {
			work.summaryBusyStartedAtMs = nil
		}
		if work.hookIdleGen != nil && work.summaryCommitGen > *work.hookIdleGen {
			work.hookIdleGen = nil
		}
		if r.snapshot.ActivePrompt == nil && r.snapshot.AutonomousHarnessTurn() != nil {
			if summary.Status.IsActive() {
				work.harnessTurnSawRunning = true
			} else if work.harnessTurnSawRunning &&
				work.summaryStatus != nil && *work.summaryStatus == AntigravityRunStatusIdle &&
				work.pendingTurnSettle == nil {
				reason := "end_turn"
				work.pendingTurnSettle = &reason
			}
		}
	}
	if pending := work.pendingStopWork; pending != nil {
		summary := snapshot.Summary
		if len(work.runningTasks) != 0 ||
			summary != nil && (summary.NotFullyIdle || summary.Status.IsActive() || summary.HasRunningSubagents) {
			pending.sawBusy = true
		} else if summary != nil && snapshot.ConversationDBPresent && summary.Status == AntigravityRunStatusIdle &&
			(pending.sawBusy ||
				(!antigravityDigestEqual(snapshot.SummaryRowDigest, pending.baselineDigest) &&
					(pending.minStepCount == nil || summary.StepCount >= *pending.minStepCount))) {
			work.pendingStopWork = nil
		}
	}
	if err := r.noteBackgroundWork(); err != nil {
		return false, err
	}
	if err := r.persistActivityTransition(); err != nil {
		return false, err
	}
	return true, nil
}

// antigravityDigestEqual is Option<[u8; 32]> equality.
func antigravityDigestEqual(left, right *[32]byte) bool {
	if left == nil || right == nil {
		return left == nil && right == nil
	}
	return *left == *right
}

func (r *DurableRelay) AntigravityTasksUnavailable() error {
	if r.backgroundWork == BackgroundWorkPolicyAntigravityTasks {
		known := false
		r.backgroundWorkKnown = &known
		return r.persistActivityTransition()
	}
	return nil
}

func (r *DurableRelay) HasPendingAntigravityTurnSettle() bool {
	return r.antigravityWork.pendingTurnSettle != nil
}

func (r *DurableRelay) SettlePendingAntigravityHarnessTurn() (bool, error) {
	if r.WorkerWritesHeld() {
		return false, nil
	}
	reason := r.antigravityWork.pendingTurnSettle
	r.antigravityWork.pendingTurnSettle = nil
	if reason != nil && r.snapshot.ActivePrompt == nil && r.snapshot.AutonomousHarnessTurn() != nil {
		origin := "antigravity"
		if err := r.settleHarnessTurn(&origin, *reason); err != nil {
			return false, err
		}
		r.antigravityWork.harnessTurnSawRunning = false
		return true, nil
	}
	return false, nil
}

// noteTaskSettled records when a Claude background task completed or
// failed, while no turn is open to answer it.
func (r *DurableRelay) noteTaskSettled() {
	if r.snapshot.ActivePrompt == nil && r.snapshot.HarnessTurn == nil {
		now := core.EpochMillis()
		r.taskSettledAtMs = &now
	}
}

// BackgroundTaskStopTarget resolves a public task id only while it still
// names stoppable background work in this process.
func (r *DurableRelay) BackgroundTaskStopTarget(requestedID string) (core.BackgroundTaskStopTarget, error) {
	if child, ok := strings.CutPrefix(requestedID, "native-agent:"); ok {
		agent, known := r.snapshot.NativeAgents[child]
		if !known || agent.State != core.NativeAgentStateRunning || !agent.Capabilities.Cancel {
			return nil, errors.New("native agent is no longer cancellable")
		}
		return core.BackgroundTaskStopTargetNativeAgent{SessionID: agent.SessionID}, nil
	}
	commands := r.backgroundCommands()
	index := slices.IndexFunc(commands, func(command core.BackgroundCommand) bool { return command.ID == requestedID })
	if index < 0 {
		return nil, errors.New("background task is no longer running")
	}
	if !commands[index].CanStop {
		return nil, errors.New("this background task cannot be stopped by its agent")
	}
	for _, terminalID := range slices.Sorted(maps.Keys(r.activeAgentTerminals)) {
		if backgroundTaskID("terminal", terminalID) == requestedID {
			return core.BackgroundTaskStopTargetHostedTerminal{TerminalID: terminalID}, nil
		}
	}
	for _, key := range slices.Sorted(maps.Keys(r.airTasks)) {
		entry := r.airTasks[key]
		command := entry.command()
		if command == nil || command.ID != requestedID {
			continue
		}
		if r.harnessTurns == HarnessTurnPolicyClaudeAdapter {
			pending, ok := r.claudePendingStops[entry.sessionID]
			if !ok {
				pending = map[string]struct{}{}
				r.claudePendingStops[entry.sessionID] = pending
			}
			pending[key] = struct{}{}
		}
		return core.BackgroundTaskStopTargetAirAsyncTask{SessionID: entry.sessionID, TaskID: entry.taskID}, nil
	}
	return nil, errors.New("background task is no longer running")
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
	if !r.isClaudeStopAcknowledgementFor(sessionID, update) || sessionID == nil {
		return false
	}
	pending, ok := r.claudePendingStops[*sessionID]
	if !ok {
		return false
	}
	// BTreeSet::pop_first: the smallest pending stop answers first.
	consumed := len(pending) > 0
	if consumed {
		delete(pending, slices.Min(slices.Collect(maps.Keys(pending))))
	}
	if len(pending) == 0 {
		delete(r.claudePendingStops, *sessionID)
	}
	return consumed
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
	failed := acpschema.ToolCallStatusFailed
	_, settled := r.RecordSessionUpdate(acpschema.SessionUpdate{
		Kind: acpschema.SessionUpdateKindToolCallUpdate,
		ToolCallUpdate: &acpschema.ToolCallUpdate{
			ToolCallID: acpschema.ToolCallId(core.FallbackTerminalToolCallID(id)),
			Status:     &failed,
		},
	})
	// A failed journal write must not retain a process that never existed.
	released := r.AgentTerminalClosed(id)
	r.serviceLiveBaselines()
	switch {
	case settled != nil && released != nil:
		return relayWithContext(settled, fmt.Sprintf("withdrawing failed terminal: %v", released))
	case settled != nil:
		return settled
	default:
		return released
	}
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
	for id := range r.activeAgentTerminals {
		if terminalRegistryOwnsTerminal(connectionID, id) {
			r.foregroundTools.Close(core.FallbackTerminalToolCallID(id))
			delete(r.activeAgentTerminals, id)
		}
	}
	for id := range r.closedAgentTerminals {
		if terminalRegistryOwnsTerminal(connectionID, id) {
			delete(r.closedAgentTerminals, id)
		}
	}
	return r.persistActivityTransition()
}

func (r *DurableRelay) retireHarnessProcessState() error {
	r.forgetHarnessProcesses()
	return r.persistActivityTransition()
}

// forgetHarnessProcesses: a restart or a close ends the harness process that
// owned whatever it had left running, so nothing it reported is still alive.
func (r *DurableRelay) forgetHarnessProcesses() {
	r.forgetAntigravityState()
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

// trackAgentTerminalToolCall remembers which hosted terminal a tool call
// embedded. Kimi launches a detached shell through a terminal of ours and
// reports the terminal id on the launcher card, which is the only link
// between the ACP card and the process its native journal later names.
func (r *DurableRelay) trackAgentTerminalToolCall(update *acpschema.SessionUpdate) {
	var toolCallID string
	var content []acpschema.ToolCallContent
	switch {
	case update.Kind == acpschema.SessionUpdateKindToolCall && update.ToolCall != nil:
		toolCallID, content = string(update.ToolCall.ToolCallID), update.ToolCall.Content
	case update.Kind == acpschema.SessionUpdateKindToolCallUpdate && update.ToolCallUpdate != nil:
		toolCallID, content = string(update.ToolCallUpdate.ToolCallID), update.ToolCallUpdate.Content
	default:
		return
	}
	for _, item := range content {
		if item.Kind == acpschema.ToolCallContentKindTerminal && item.Terminal != nil {
			r.agentTerminalToolCalls[kimiNativeToolCallID(toolCallID)] = string(item.Terminal.TerminalID)
		}
	}
}

// trackKimiBackgroundAgent keeps positive ACP evidence until Kimi's native
// journal proves which detached task the launcher created. Kimi completes
// the launcher card immediately, so a completed status is not a
// child-liveness boundary.
func (r *DurableRelay) trackKimiBackgroundAgent(update *acpschema.SessionUpdate) {
	var (
		toolCallID string
		title      *string
		status     *acpschema.ToolCallStatus
		rawInput   json.RawMessage
		rawOutput  json.RawMessage
	)
	switch {
	case update.Kind == acpschema.SessionUpdateKindToolCall && update.ToolCall != nil:
		call := update.ToolCall
		callTitle := call.Title
		// A tool call without a status is pending, its ACP default.
		callStatus := acpschema.ToolCallStatusPending
		if call.Status != nil {
			callStatus = *call.Status
		}
		toolCallID, title, status = string(call.ToolCallID), &callTitle, &callStatus
		rawInput, rawOutput = call.RawInput, call.RawOutput
	case update.Kind == acpschema.SessionUpdateKindToolCallUpdate && update.ToolCallUpdate != nil:
		call := update.ToolCallUpdate
		toolCallID, title, status = string(call.ToolCallID), call.Title, call.Status
		rawInput, rawOutput = call.RawInput, call.RawOutput
	default:
		return
	}
	if status != nil && *status == acpschema.ToolCallStatusFailed {
		delete(r.kimiProvisionalTasks, toolCallID)
		return
	}
	taskID, hasTaskID := kimiRunningTaskID(rawOutput)
	explicitBackground := false
	if len(rawInput) > 0 {
		var input map[string]any
		if json.Unmarshal(rawInput, &input) == nil {
			background, ok := input["run_in_background"].(bool)
			explicitBackground = ok && background
		}
	}
	backgroundTitle := title != nil && strings.HasPrefix(*title, "Launching background ")
	if !explicitBackground && !backgroundTitle && !hasTaskID {
		return
	}
	_, observedTool := r.kimiObservedToolIDs[kimiNativeToolCallID(toolCallID)]
	_, observedTask := r.kimiObservedTaskIDs[taskID]
	if observedTool || (hasTaskID && observedTask) {
		delete(r.kimiProvisionalTasks, toolCallID)
		return
	}
	// Several TaskOutput/WaitFor calls may observe the same native task. Keep
	// one provisional entry until its lifecycle stream catches up.
	startedAtMs := core.EpochMillis()
	if step := r.stepClock.StartedAtMs(); step != nil {
		startedAtMs = *step
	}
	if hasTaskID {
		for existingCall, task := range r.kimiProvisionalTasks {
			if task.nativeTaskID != nil && *task.nativeTaskID == taskID {
				startedAtMs = min(startedAtMs, task.command.StartedAtMs)
				if existingCall != toolCallID {
					delete(r.kimiProvisionalTasks, existingCall)
				}
			}
		}
	}
	var nativeTaskID *string
	if hasTaskID {
		nativeTaskID = &taskID
	}
	command := kimiAgentDescription(title, rawInput, nativeTaskID)
	if task, ok := r.kimiProvisionalTasks[toolCallID]; ok {
		task.command.Command = command
		if hasTaskID {
			id := taskID
			task.nativeTaskID = &id
		}
		r.kimiProvisionalTasks[toolCallID] = task
		return
	}
	var provisionalTaskID *string
	if hasTaskID {
		id := taskID
		provisionalTaskID = &id
	}
	r.kimiProvisionalTasks[toolCallID] = KimiProvisionalTask{
		command: core.BackgroundCommand{
			ID:          backgroundTaskID("kimi-provisional", toolCallID),
			StartedAtMs: startedAtMs,
			Command:     command,
			CanStop:     false,
		},
		nativeTaskID: provisionalTaskID,
	}
}

// Port of mj-worker/src/relay/background.rs.
//
// Agent-owned background work: adapter (AIR) tasks, Kimi's detached agents
// and the hosted terminals the agent left running.

package worker

import (
	"cmp"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"slices"
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

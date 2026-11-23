// Port of mj-transcript/src/projection/observation.rs.

package transcript

import (
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"reflect"
	"slices"
	"sort"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// externalEventsItemPrefix: sessions projected before delivery rows existed
// keep one accumulated row under `system:mailbox:external-events`. Delivery
// rows use this prefix and an event key, so the two can never share an
// identity.
const externalEventsItemPrefix = "system:mailbox:delivered:"

// QueuedCommandContent is the queue presentation shared by ordinary replay,
// current-state recovery and archive restore: ACP content blocks in their
// JSON form. A mailbox wake keeps no payload here: its messages are rows of
// their own, the rendered prompt can exceed a transcript turn's budgets, and
// the worker's queue holds it. The command ID decides, because a restored or
// replayed wake is a plain prompt under its wake ID.
func QueuedCommandContent(commandID string, command core.RelayCommand) ([]json.RawMessage, error) {
	if core.CarriesMailboxWake(commandID) {
		return []json.RawMessage{}, nil
	}
	if config, ok := command.(core.RelayCommandSetConfig); ok {
		block, err := projectionJSON(textBlock(core.ConfigCommandText(config.Key, config.Value)))
		if err != nil {
			return nil, err
		}
		return []json.RawMessage{block}, nil
	}
	prompt, ok := core.RelayCommandPromptBlocks(command)
	if !ok {
		return nil, errors.New("command is not a queued prompt")
	}
	content := make([]json.RawMessage, 0, len(prompt))
	for _, block := range prompt {
		encoded, err := projectionJSON(block)
		if err != nil {
			return nil, err
		}
		content = append(content, encoded)
	}
	return content, nil
}

// queuedContentValues is content in the form core.MaterializedQueuedPrompt
// holds it (Rust's Vec<Value>): each block's JSON text is one element.
func queuedContentValues(content []json.RawMessage) []any {
	values := make([]any, len(content))
	for i, block := range content {
		values[i] = block
	}
	return values
}

// queuedContentRaw is a queue entry's content as the JSON blocks transcript
// bodies hold.
func queuedContentRaw(content []any) ([]json.RawMessage, error) {
	raw := make([]json.RawMessage, len(content))
	for i, value := range content {
		encoded, err := queuedBlockJSON(value)
		if err != nil {
			return nil, err
		}
		raw[i] = encoded
	}
	return raw, nil
}

// queuedBlockJSON is one queued content block's JSON text; a block already
// held as JSON text is kept byte for byte.
func queuedBlockJSON(value any) (json.RawMessage, error) {
	if raw, ok := value.(json.RawMessage); ok {
		return raw, nil
	}
	return projectionJSON(value)
}

// queuedBlockText is the "text" member of a queued content block, if it is
// a string.
func queuedBlockText(value any) (string, bool) {
	encoded, err := queuedBlockJSON(value)
	if err != nil {
		return "", false
	}
	parsed, err := jsonvalue.Parse(encoded)
	if err != nil {
		return "", false
	}
	object, ok := parsed.(jsonvalue.Object)
	if !ok {
		return "", false
	}
	member, ok := object.Get("text")
	if !ok {
		return "", false
	}
	text, ok := member.(string)
	return text, ok
}

// queuedIsPrompt reports whether a queue entry is a prompt; an absent kind
// is one.
func queuedIsPrompt(prompt *core.MaterializedQueuedPrompt) bool {
	return prompt.Kind == nil || prompt.Kind.IsPrompt()
}

// queuedPromptsEqual is the Rust PartialEq of two queues.
func queuedPromptsEqual(left, right []core.MaterializedQueuedPrompt) bool {
	return reflect.DeepEqual(left, right)
}

// cloneSessionConfiguration is SessionConfiguration::clone: the values map
// is copied (and always present, so it can be written), the goal is shared
// because the projection replaces it rather than changing it in place.
func cloneSessionConfiguration(configuration core.SessionConfiguration) core.SessionConfiguration {
	values := make(map[string]any, len(configuration.Values))
	maps.Copy(values, configuration.Values)
	return core.SessionConfiguration{Goal: configuration.Goal, Values: values}
}

// keepHandoffState keeps the hidden handoff state the harness cannot see
// across a configuration replacement: the projected value is dropped and the
// current one restored.
func keepHandoffState(current *core.MaterializedSession, configuration *core.SessionConfiguration) {
	delete(configuration.Values, HandoffStateKey)
	if state, ok := current.Configuration.Values[HandoffStateKey]; ok {
		configuration.Values[HandoffStateKey] = state
	}
}

func projectSteeredPrompt(
	current *core.MaterializedSession,
	index *ProjectionIndex,
	event *core.RelayEvent,
	queue *[]core.MaterializedQueuedPrompt,
	queuedPromptID string,
	mutation *core.MaterializedSessionMutation,
) error {
	queueIndex := slices.IndexFunc(*queue, func(queued core.MaterializedQueuedPrompt) bool {
		return queued.CommandID == queuedPromptID
	})
	if queueIndex < 0 {
		return errors.New("steered prompt is missing from the materialized queue")
	}
	entry := (*queue)[queueIndex]
	*queue = slices.Delete(slices.Clone(*queue), queueIndex, queueIndex+1)
	if !queuedIsPrompt(&entry) {
		return errors.New("steered queue entry is not a prompt")
	}
	// The running turn becomes the steered prompt's turn, while the original
	// harness turn remains the one whose completion closes it.
	var steeredInto *string
	if running := current.ActiveTurn; running != nil {
		original := running.CommandID
		if running.SteeredInto != nil {
			original = *running.SteeredInto
		}
		steeredInto = &original
	}
	turn := &core.MaterializedTurn{
		CommandID:         queuedPromptID,
		AcceptedOrdinal:   entry.AcceptedOrdinal,
		TurnStartPosition: event.Ordinal,
		StartedAtMs:       event.RecordedAtMs,
		SteeredInto:       steeredInto,
	}
	mutation.ActiveTurn = &turn
	closeStreams(index, mutation, event.RecordedAtMs)
	content, err := queuedContentRaw(entry.Content)
	if err != nil {
		return err
	}
	upsert(mutation, core.TranscriptItem{
		StableID:        "user:" + queuedPromptID,
		Position:        event.Ordinal,
		CreatedAtMs:     event.RecordedAtMs,
		LastChangedAtMs: event.RecordedAtMs,
		Body:            core.TranscriptBodyUser{Content: content},
	})
	return nil
}

func setExecution(mutation *core.MaterializedSessionMutation, execution core.MaterializedExecutionState) {
	mutation.Execution = &execution
}

func setQueue(mutation *core.MaterializedSessionMutation, queue []core.MaterializedQueuedPrompt) {
	mutation.QueuedPrompts = &queue
}

func setPendingElicitations(mutation *core.MaterializedSessionMutation, pending []core.ElicitationRequest) {
	mutation.PendingElicitations = &pending
}

func clearActiveTurn(mutation *core.MaterializedSessionMutation) {
	var none *core.MaterializedTurn
	mutation.ActiveTurn = &none
}

// goalRunning reports whether the projected native goal is executing.
func goalRunning(configuration *core.SessionConfiguration) bool {
	return configuration.Goal != nil && configuration.Goal.Running()
}

func projectObservation(current *core.MaterializedSession, index *ProjectionIndex, event *core.RelayEvent, mutation *core.MaterializedSessionMutation) error {
	switch observation := event.Observation.(type) {
	case core.RelayObservationSteeringUnconfirmed:
	case core.RelayObservationNativeAgent:
		native := *event
		mutation.NativeAgent = &native
	case core.RelayObservationAgentInitialized:
	case core.RelayObservationSessionOpened:
		if !observation.Resumed {
			pushSystem(mutation, event, "harness session started")
		}
	case core.RelayObservationSessionConfigured:
		configuration := cloneSessionConfiguration(current.Configuration)
		values, err := ConfigurationValues(observation.ConfigOptions)
		if err != nil {
			return err
		}
		configuration.Values = values
		keepHandoffState(current, &configuration)
		mutation.Configuration = &configuration
	case core.RelayObservationSessionModesConfigured:
	case core.RelayObservationSessionUpdate:
		if err := projectSessionUpdate(current, index, event, &observation.Update, mutation); err != nil {
			return err
		}
		if mutation.Configuration != nil {
			keepHandoffState(current, mutation.Configuration)
		}
	case core.RelayObservationPermissionAutoApproved:
		pushSystem(mutation, event, fmt.Sprintf("permission auto-approved: %s (%s)", observation.OptionName, observation.OptionID))
	case core.RelayObservationElicitationRequested:
		request := observation.Request
		pending := slices.DeleteFunc(slices.Clone(current.PendingElicitations), func(existing core.ElicitationRequest) bool {
			return existing.ID == request.ID
		})
		pending = append(pending, request)
		setPendingElicitations(mutation, pending)
		// A plan decision also becomes a durable transcript item at the
		// point the harness proposed it, so the proposal renders inline
		// after the conversation that produced it and outlives both the
		// decision dialog and the session's process.
		if plan, ok := core.PlanReviewProposal(&request); ok {
			closeStreams(index, mutation, event.RecordedAtMs)
			upsert(mutation, core.TranscriptItem{
				StableID:        PlanProposalItemID(event.Ordinal),
				Position:        event.Ordinal,
				CreatedAtMs:     event.RecordedAtMs,
				LastChangedAtMs: event.RecordedAtMs,
				Body:            core.TranscriptBodyPlanProposal{ProposalID: request.ID, Plan: plan},
			})
		}
	case core.RelayObservationElicitationResolved:
		if observation.Action == core.OperatorAwayAction {
			for i := range current.PendingElicitations {
				request := &current.PendingElicitations[i]
				if request.ID != observation.ElicitationID {
					continue
				}
				title, _ := core.NormalizeSessionTitle(request.Message)
				pushSystem(mutation, event, "Not shown because you were away: "+title)
				break
			}
		} else if reply := observation.Reply; reply != nil {
			closeStreams(index, mutation, event.RecordedAtMs)
			content, err := projectionJSON(textBlock(*reply))
			if err != nil {
				return err
			}
			upsert(mutation, core.TranscriptItem{
				StableID:        fmt.Sprintf("%s%d", core.ElicitationReplyItemPrefix, event.Ordinal),
				Position:        event.Ordinal,
				CreatedAtMs:     event.RecordedAtMs,
				LastChangedAtMs: event.RecordedAtMs,
				Body:            core.TranscriptBodyUser{Content: []json.RawMessage{content}},
			})
		}
		pending := slices.DeleteFunc(slices.Clone(current.PendingElicitations), func(request core.ElicitationRequest) bool {
			return request.ID == observation.ElicitationID
		})
		setPendingElicitations(mutation, pending)
	case core.RelayObservationElicitationsCleared:
		setPendingElicitations(mutation, []core.ElicitationRequest{})
	case core.RelayObservationCommandQueued:
		if err := projectCommandQueued(current, index, event, &observation, mutation); err != nil {
			return err
		}
	case core.RelayObservationCommandStarted:
		if err := projectCommandStarted(current, index, event, &observation, mutation); err != nil {
			return err
		}
	case core.RelayObservationCommandCompleted:
		if err := projectCommandCompleted(current, index, event, &observation, mutation); err != nil {
			return err
		}
	case core.RelayObservationCommandRejected:
		projectCommandFailure(current, index, event, commandFailure{
			commandID: observation.CommandID,
			command:   observation.Command,
			message:   observation.Message,
			reason:    observation.Reason,
			rejected:  true,
		}, mutation)
	case core.RelayObservationCommandInterrupted:
		projectCommandFailure(current, index, event, commandFailure{
			commandID:      observation.CommandID,
			command:        observation.Command,
			message:        observation.Message,
			reason:         observation.Reason,
			continuationID: observation.ContinuationID,
		}, mutation)
	case core.RelayObservationConfigurationUpdated:
		if observation.Key == HandoffStateKey {
			slog.Warn("harness attempted to change reserved handoff state key")
			return nil
		}
		configuration := cloneSessionConfiguration(current.Configuration)
		configuration.Values[observation.Key] = observation.Value
		mutation.Configuration = &configuration
	case core.RelayObservationCheckpointReady:
	case core.RelayObservationUserShellOutput:
		if existing := index.get(userShellItemID(observation.CommandID)); existing != nil {
			item := cloneTranscriptItem(existing)
			item.Body = core.TranscriptBodySystem{Text: userShellText(
				observation.Command, "running", observation.Stdout, observation.Stderr,
				observation.StdoutTruncated, observation.StderrTruncated)}
			item.LastChangedAtMs = max(item.LastChangedAtMs, event.RecordedAtMs)
			upsert(mutation, item)
		}
	// Terminal output can land before or after the tool call that names the
	// terminal, so both orderings have to end in the same place: attached to
	// every referencing tool item, or parked in a standalone item that the
	// tool call consumes when it arrives.
	case core.RelayObservationTerminalOutput:
		if err := projectTerminalOutput(index, event, &observation, mutation); err != nil {
			return err
		}
	case core.RelayObservationWarning:
		pushSystem(mutation, event, "warning: "+observation.Message)
	case core.RelayObservationSessionFault:
		pushSystem(mutation, event, "warning: "+observation.Message)
	case core.RelayObservationSessionRestarted:
		setPendingElicitations(mutation, []core.ElicitationRequest{})
		configuration := cloneSessionConfiguration(current.Configuration)
		if configuration.Goal != nil {
			goal := *configuration.Goal
			goal.Restart()
			configuration.Goal = &goal
			mutation.Configuration = &configuration
		}
		pushSystemWithID(mutation, event,
			fmt.Sprintf("%s%d", core.SessionRestartItemPrefix, event.Ordinal), core.SessionRestartText)
		// A restart during a turn the harness started on its own leaves
		// nothing that can finish it. Without this the session stays
		// Running with open streams, which canonical export refuses.
		if _, running := current.Execution.(core.MaterializedExecutionStateRunning); running && current.ActiveTurn == nil {
			pushSystemWithID(mutation, event,
				fmt.Sprintf("%s%d", core.WorkInterruptedItemPrefix, event.Ordinal),
				"Work interrupted by session restart")
			closeStreams(index, mutation, event.RecordedAtMs)
			setExecution(mutation, core.MaterializedExecutionStateIdle{})
		}
	case core.RelayObservationHarnessTurnStarted:
		// Codex reports native execution starts for ordinary replies too.
		// The user turn already supplies the transcript boundary and clock.
		if current.ActiveTurn != nil {
			break
		}
		upsert(mutation, core.TranscriptItem{
			StableID:        fmt.Sprintf("%s%d", core.HarnessTurnItemPrefix, event.Ordinal),
			Position:        event.Ordinal,
			CreatedAtMs:     event.RecordedAtMs,
			LastChangedAtMs: event.RecordedAtMs,
			Body:            core.TranscriptBodySystem{Text: core.HarnessTurnText},
		})
		setExecution(mutation, core.MaterializedExecutionStateRunning{StartedAtMs: observation.StartedAtMs})
	case core.RelayObservationHarnessTurnSettled:
		// A prompt dispatched mid-turn is still running when the turn the
		// harness started on its own settles. The relay keeps the session
		// Running for it, and so does this: the prompt's own result closes
		// the streams and stops the clock.
		if observation.PromptInFlight {
			break
		}
		if turn := observation.Autonomous; turn != nil {
			start := turn.StartOrdinal
			commandID := core.HarnessTurnID(start)
			// Since relay protocol 51, settles report cancellations. Repair
			// replay of older journals records no Interrupted row or
			// cancelled harness outcome (session_manager/standalone.rs
			// repair_projection).
			if core.ClassifyPromptCompletion(turn.StopReason) == core.PromptCompletionCancelled {
				stableID := core.TurnInterruptedItemPrefix + commandID
				if index.get(stableID) == nil {
					pushSystemWithID(mutation, event, stableID, core.TurnInterruptedText)
				}
			}
			if current.LastTurnOutcome == nil || current.LastTurnOutcome.CommandID != commandID {
				var workFacts *core.WorkFacts
				if turn.WorkFacts != nil {
					facts := *turn.WorkFacts
					workFacts = &facts
				}
				startPosition := start
				mutation.LastTurnOutcome = &core.MaterializedTurnOutcome{
					WorkFacts:         workFacts,
					CommandID:         commandID,
					TurnStartPosition: &startPosition,
					CompletedOrdinal:  event.Ordinal,
					CompletedAtMs:     event.RecordedAtMs,
					Outcome:           core.TurnOutcomeKindCompleted{StopReason: turn.StopReason},
				}
			}
		}
		closeStreams(index, mutation, event.RecordedAtMs)
		setExecution(mutation, core.MaterializedExecutionStateIdle{})
	// Keyed on the command rather than the event ordinal, and skipped once
	// the line exists: a relay that re-records the same notice after a
	// persistence retry leaves exactly one line in the conversation.
	case core.RelayObservationNotice:
		stableID := fmt.Sprintf("system:%d", event.Ordinal)
		if event.CommandID != nil {
			stableID = "system:notice:" + *event.CommandID
		}
		if index.get(stableID) == nil {
			pushSystemWithID(mutation, event, stableID, observation.Message)
		}
	case core.RelayObservationMailboxEventsDelivered:
		deliveredMessages(index, event, observation.Events, mutation)
		deliveredExternalEvents(index, event, observation.Events, mutation)
		if steering := observation.Steering; steering != nil {
			queue := slices.Clone(current.QueuedPrompts)
			if err := projectSteeredPrompt(current, index, event, &queue, steering.QueuedPromptID, mutation); err != nil {
				return err
			}
			setQueue(mutation, queue)
		}
	case core.RelayObservationMailboxHookLeaseCreated, core.RelayObservationMailboxHookLeaseReturned:
	case core.RelayObservationClosing:
		closeStreams(index, mutation, event.RecordedAtMs)
		setExecution(mutation, core.MaterializedExecutionStateClosing{})
	case core.RelayObservationClosed:
		closeStreams(index, mutation, event.RecordedAtMs)
		setExecution(mutation, core.MaterializedExecutionStateClosed{})
	case core.RelayObservationManagedSubagentConfigured,
		core.RelayObservationWorkLivenessChanged,
		core.RelayObservationRetryAssessmentStarted,
		core.RelayObservationHandbackCheckStarted,
		core.RelayObservationQuestionTargetUpdated,
		core.RelayObservationQuestionLocatorFinished,
		core.RelayObservationTurnAssessmentUpdated,
		core.RelayObservationRetryAssessmentResolved:
	}
	if outcome := mutation.LastTurnOutcome; outcome != nil {
		outcome.AnsweredThrough = answeredThrough(current, event, outcome.CommandID)
	}
	return nil
}

func projectCommandQueued(
	current *core.MaterializedSession,
	index *ProjectionIndex,
	event *core.RelayEvent,
	observation *core.RelayObservationCommandQueued,
	mutation *core.MaterializedSessionMutation,
) error {
	commandID := observation.CommandID
	switch command := observation.Command.(type) {
	case core.RelayCommandInstallPromptContext:
		if state, ok := ParseHandoff(command.Text); ok {
			encoded, err := projectionJSON(state)
			if err != nil {
				return err
			}
			value, err := jsonvalue.Parse(encoded)
			if err != nil {
				return err
			}
			configuration := cloneSessionConfiguration(current.Configuration)
			configuration.Values[HandoffStateKey] = value
			mutation.Configuration = &configuration
		}
		return nil
	case core.RelayCommandClearContext:
		setExecution(mutation, core.MaterializedExecutionStateRunning{StartedAtMs: observation.CreatedAtMs})
		pushSystem(mutation, event, "Clearing context…")
		return nil
	}
	// Every command that carries a prompt starts a user turn, so the
	// transcript shows what the harness was told. Mjolnir's own prompts (a
	// handback reminder, a quota resume) included.
	if _, prompt := core.RelayCommandPromptBlocks(observation.Command); prompt {
		if continued, ok := observation.Command.(core.RelayCommandContinueAuthorizedWork); ok {
			pushSystem(mutation, event, fmt.Sprintf("Continuing requested work automatically · %d of 3", continued.Attempt))
		}
		if wake, ok := observation.Command.(core.RelayCommandMailboxWake); ok {
			deliveredMessages(index, event, wake.Events, mutation)
			deliveredExternalEvents(index, event, wake.Events, mutation)
		}
		content, err := QueuedCommandContent(commandID, observation.Command)
		if err != nil {
			return err
		}
		if current.SessionTitle == nil && !core.CarriesMailboxWake(commandID) {
			title, ok := current.ResolvedTitle()
			if !ok {
				title, ok = core.ProvisionalSessionTitle(core.MaterializedContentText(content))
			}
			if ok {
				titled := &title
				mutation.SessionTitle = &titled
			}
		}
		queue := slices.DeleteFunc(slices.Clone(current.QueuedPrompts), func(queued core.MaterializedQueuedPrompt) bool {
			return queued.CommandID == commandID
		})
		accepted := event.Ordinal
		queue = append(queue, core.MaterializedQueuedPrompt{
			CommandID:       commandID,
			Kind:            core.QueuedCommandKindPrompt{},
			Content:         queuedContentValues(content),
			QueuedAtMs:      observation.CreatedAtMs,
			AcceptedOrdinal: &accepted,
		})
		setQueue(mutation, queue)
		return nil
	}
	switch command := observation.Command.(type) {
	// A configuration change waits in the same queue as prompts and is
	// displayed as the composer text that produced it.
	case core.RelayCommandSetConfig:
		content, err := QueuedCommandContent(commandID, command)
		if err != nil {
			return err
		}
		queue := slices.DeleteFunc(slices.Clone(current.QueuedPrompts), func(queued core.MaterializedQueuedPrompt) bool {
			return queued.CommandID == commandID
		})
		accepted := event.Ordinal
		queue = append(queue, core.MaterializedQueuedPrompt{
			CommandID:       commandID,
			Kind:            core.QueuedCommandKindSetConfig{Key: command.Key, Value: command.Value},
			Content:         queuedContentValues(content),
			QueuedAtMs:      observation.CreatedAtMs,
			AcceptedOrdinal: &accepted,
		})
		setQueue(mutation, queue)
	case core.RelayCommandRunUserShell:
		upsert(mutation, core.TranscriptItem{
			StableID:        userShellItemID(commandID),
			Position:        event.Ordinal,
			CreatedAtMs:     observation.CreatedAtMs,
			LastChangedAtMs: observation.CreatedAtMs,
			Body:            core.TranscriptBodySystem{Text: userShellText(command.Command, "queued", "", "", false, false)},
		})
	// A queued event is not yet something the agent has seen. It appears in
	// the transcript when a wake, prompt, or tool hook delivers it, next to
	// the work it caused.
	case core.RelayCommandDeliverMailboxEvent:
	case core.RelayCommandRemoveQueuedPrompt, core.RelayCommandClearQueuedPrompts:
	case core.RelayCommandClose:
		closeStreams(index, mutation, event.RecordedAtMs)
		setExecution(mutation, core.MaterializedExecutionStateClosing{})
	}
	return nil
}

func projectCommandStarted(
	current *core.MaterializedSession,
	index *ProjectionIndex,
	event *core.RelayEvent,
	observation *core.RelayObservationCommandStarted,
	mutation *core.MaterializedSessionMutation,
) error {
	commandID := observation.CommandID
	startedAtMs := observation.StartedAtMs
	queueIndex := slices.IndexFunc(current.QueuedPrompts, func(queued core.MaterializedQueuedPrompt) bool {
		return queued.CommandID == commandID
	})
	if queueIndex >= 0 {
		queue := slices.Clone(current.QueuedPrompts)
		entry := queue[queueIndex]
		queue = slices.Delete(queue, queueIndex, queueIndex+1)
		entry.Content = slices.Clone(entry.Content)
		slash := false
		if len(entry.Content) > 0 {
			if text, ok := queuedBlockText(entry.Content[0]); ok {
				slash = core.PromptIsSlashCommand([]schema.ContentBlock{textBlock(text)})
			}
		}
		if core.IsRestartContinuation(commandID) && !slash {
			note, err := projectionJSON(textBlock(core.RestartRecoveryNote))
			if err != nil {
				return err
			}
			entry.Content = []any{note}
		}
		entryAcceptedOrdinal := entry.AcceptedOrdinal
		setQueue(mutation, queue)
		// A configuration change applies between turns: it never becomes a
		// transcript turn and never starts the turn clock.
		if queuedIsPrompt(&entry) {
			closeStreams(index, mutation, event.RecordedAtMs)
			item := core.TranscriptItem{
				Position:        event.Ordinal,
				CreatedAtMs:     startedAtMs,
				LastChangedAtMs: startedAtMs,
			}
			if len(entry.Content) == 0 {
				item.StableID = core.HarnessTurnItemPrefix + commandID
				item.Body = core.TranscriptBodySystem{Text: core.HarnessTurnText}
			} else {
				content, err := queuedContentRaw(entry.Content)
				if err != nil {
					return err
				}
				item.StableID = "user:" + commandID
				item.Body = core.TranscriptBodyUser{Content: content}
			}
			upsert(mutation, item)
			setExecution(mutation, core.MaterializedExecutionStateRunning{StartedAtMs: startedAtMs})
			turn := &core.MaterializedTurn{
				CommandID:         commandID,
				AcceptedOrdinal:   entryAcceptedOrdinal,
				TurnStartPosition: event.Ordinal,
				StartedAtMs:       startedAtMs,
			}
			mutation.ActiveTurn = &turn
		}
	}
	if existing := index.get(userShellItemID(commandID)); existing != nil {
		item := cloneTranscriptItem(existing)
		if body, ok := item.Body.(core.TranscriptBodySystem); ok {
			body.Text = strings.Replace(body.Text, "Shell · queued", "Shell · running", 1)
			item.Body = body
		}
		item.LastChangedAtMs = max(item.LastChangedAtMs, startedAtMs)
		upsert(mutation, item)
	}
	return nil
}

func projectCommandCompleted(
	current *core.MaterializedSession,
	index *ProjectionIndex,
	event *core.RelayEvent,
	observation *core.RelayObservationCommandCompleted,
	mutation *core.MaterializedSessionMutation,
) error {
	commandID := observation.CommandID
	queue := slices.DeleteFunc(slices.Clone(current.QueuedPrompts), func(queued core.MaterializedQueuedPrompt) bool {
		return queued.CommandID == commandID
	})
	switch outcome := observation.Outcome.(type) {
	case core.RelayCommandOutcomeContextCleared:
		mutation.ClearTurnOutcome = true
		mutation.ClearMailbox = true
		closeStreams(index, mutation, event.RecordedAtMs)
		setExecution(mutation, core.MaterializedExecutionStateIdle{})
		clearActiveTurn(mutation)
		setPendingElicitations(mutation, []core.ElicitationRequest{})
		configuration := cloneSessionConfiguration(current.Configuration)
		var capability *core.GoalCapability
		if configuration.Goal != nil {
			capability = configuration.Goal.Capability
		}
		configuration.Goal = &core.GoalState{Capability: capability, Known: true}
		mutation.Configuration = &configuration
		upsert(mutation, core.TranscriptItem{
			StableID:        core.ContextBoundaryPrefix + commandID,
			Position:        event.Ordinal,
			CreatedAtMs:     event.RecordedAtMs,
			LastChangedAtMs: event.RecordedAtMs,
			Body:            core.TranscriptBodySystem{Text: "Context cleared — a new conversation starts here."},
		})
	case core.RelayCommandOutcomePrompt:
		if !goalRunning(&current.Configuration) {
			closeStreams(index, mutation, event.RecordedAtMs)
			setExecution(mutation, core.MaterializedExecutionStateIdle{})
		}
		// A cancelled turn's reply stops mid-sentence; mark where it ended
		// so it does not read as a finished reply.
		if core.ClassifyPromptCompletion(outcome.StopReason) == core.PromptCompletionCancelled {
			pushSystemWithID(mutation, event, core.TurnInterruptedItemPrefix+commandID, core.TurnInterruptedText)
		}
		var active *core.MaterializedTurn
		if current.ActiveTurn != nil && current.ActiveTurn.BelongsTo(commandID) {
			active = current.ActiveTurn
		}
		turnOutcome := &core.MaterializedTurnOutcome{
			Diagnostic:       cloneDiagnostic(outcome.Diagnostic),
			Usage:            cloneUsage(outcome.Usage),
			CommandID:        commandID,
			CompletedOrdinal: event.Ordinal,
			CompletedAtMs:    event.RecordedAtMs,
			Outcome:          core.TurnOutcomeKindCompleted{StopReason: outcome.StopReason},
		}
		if outcome.WorkFacts != nil {
			facts := *outcome.WorkFacts
			turnOutcome.WorkFacts = &facts
		}
		if active != nil {
			turnOutcome.AcceptedOrdinal = active.AcceptedOrdinal
			start := active.TurnStartPosition
			turnOutcome.TurnStartPosition = &start
		}
		mutation.LastTurnOutcome = turnOutcome
		clearActiveTurn(mutation)
	case core.RelayCommandOutcomeUserShell:
		if existing := index.get(userShellItemID(commandID)); existing != nil {
			item := cloneTranscriptItem(existing)
			item.Body = core.TranscriptBodySystem{Text: userShellResultText(&outcome.Result)}
			item.LastChangedAtMs = max(item.LastChangedAtMs, event.RecordedAtMs)
			upsert(mutation, item)
		}
	case core.RelayCommandOutcomeClosed:
		closeStreams(index, mutation, event.RecordedAtMs)
		setExecution(mutation, core.MaterializedExecutionStateClosed{})
	case core.RelayCommandOutcomeQueueChanged:
		queue = slices.DeleteFunc(queue, func(queued core.MaterializedQueuedPrompt) bool {
			return slices.Contains(outcome.RemovedCommandIds, queued.CommandID)
		})
	case core.RelayCommandOutcomeSteered:
		if err := projectSteeredPrompt(current, index, event, &queue, outcome.QueuedCommandID, mutation); err != nil {
			return err
		}
	case core.RelayCommandOutcomeConfigured:
		mutation.ConfigResults = append(mutation.ConfigResults, core.MaterializedConfigResult{ConfigID: commandID})
	case core.RelayCommandOutcomeCancelled:
	case core.RelayCommandOutcomeGoalControlled,
		core.RelayCommandOutcomeSessionModeSet,
		core.RelayCommandOutcomeCheckpointCompleted,
		core.RelayCommandOutcomeCheckpointReleased,
		core.RelayCommandOutcomeRecoveryFloorAdvanced,
		core.RelayCommandOutcomeNoticeRecorded,
		core.RelayCommandOutcomeUserShellCancelled,
		// The returned prompt keeps its queue entry.
		core.RelayCommandOutcomeSteeringReturned:
	}
	if !queuedPromptsEqual(queue, current.QueuedPrompts) {
		setQueue(mutation, queue)
	}
	return nil
}

func cloneDiagnostic(diagnostic *core.TurnDiagnostic) *core.TurnDiagnostic {
	if diagnostic == nil {
		return nil
	}
	copied := *diagnostic
	return &copied
}

func cloneUsage(usage *core.TokenUsage) *core.TokenUsage {
	if usage == nil {
		return nil
	}
	copied := *usage
	return &copied
}

// commandFailure is the shared shape of CommandRejected and
// CommandInterrupted.
type commandFailure struct {
	commandID string
	command   core.RelayCommandKind
	message   string
	reason    *core.OutcomeReason
	// rejected distinguishes CommandRejected from CommandInterrupted.
	rejected bool
	// continuationID is the interrupted prompt's recovery command, if any.
	continuationID *string
}

func projectCommandFailure(
	current *core.MaterializedSession,
	index *ProjectionIndex,
	event *core.RelayEvent,
	failure commandFailure,
	mutation *core.MaterializedSessionMutation,
) {
	commandID := failure.commandID
	command := failure.command
	message := failure.message
	if command == core.RelayCommandKindClearContext {
		setExecution(mutation, core.MaterializedExecutionStateIdle{})
	}
	if command == core.RelayCommandKindSetConfig || command == core.RelayCommandKindRestoreExecutionMode {
		text := message
		mutation.ConfigResults = append(mutation.ConfigResults, core.MaterializedConfigResult{ConfigID: commandID, Value: &text})
	}
	var active *core.MaterializedTurn
	if current.ActiveTurn != nil && current.ActiveTurn.BelongsTo(commandID) {
		active = current.ActiveTurn
	}
	promptWasStarted := active != nil || index.get("user:"+commandID) != nil
	var queuedEntry *core.MaterializedQueuedPrompt
	for i := range current.QueuedPrompts {
		if current.QueuedPrompts[i].CommandID == commandID {
			queuedEntry = &current.QueuedPrompts[i]
			break
		}
	}
	queue := slices.DeleteFunc(slices.Clone(current.QueuedPrompts), func(queued core.MaterializedQueuedPrompt) bool {
		return queued.CommandID == commandID
	})
	var acceptedOrdinal *uint64
	if active != nil {
		acceptedOrdinal = active.AcceptedOrdinal
	}
	if acceptedOrdinal == nil && queuedEntry != nil {
		acceptedOrdinal = queuedEntry.AcceptedOrdinal
	}
	if recoveryID := failure.continuationID; !failure.rejected && recoveryID != nil {
		// Mailbox turns already have their own message rows. Reusing their
		// payload must not publish or acknowledge those again.
		var content []any
		if user := index.get("user:" + commandID); user != nil {
			if body, ok := user.Body.(core.TranscriptBodyUser); ok {
				content = queuedContentValues(body.Content)
			}
		}
		if content == nil && queuedEntry != nil {
			content = slices.Clone(queuedEntry.Content)
		}
		if core.IsRestartContinuation(commandID) && len(content) > 0 {
			if text, ok := queuedBlockText(content[0]); ok && text == core.RestartRecoveryNote {
				content = content[1:]
			}
		}
		recoveryAccepted := acceptedOrdinal
		if recoveryAccepted == nil {
			ordinal := event.Ordinal
			recoveryAccepted = &ordinal
		}
		queue = slices.Insert(queue, 0, core.MaterializedQueuedPrompt{
			CommandID:       *recoveryID,
			Kind:            core.QueuedCommandKindPrompt{},
			Content:         content,
			QueuedAtMs:      event.RecordedAtMs,
			AcceptedOrdinal: recoveryAccepted,
		})
	}
	if !queuedPromptsEqual(queue, current.QueuedPrompts) {
		setQueue(mutation, queue)
	}
	// WorkerRestarted proves the bridge stopped; its last native execution
	// update cannot keep this interrupted prompt running.
	bridgeStopped := !failure.rejected && failure.reason != nil && *failure.reason == core.OutcomeReasonWorkerRestarted
	if (active != nil || (current.ActiveTurn == nil && promptWasStarted)) &&
		(bridgeStopped || !goalRunning(&current.Configuration)) {
		closeStreams(index, mutation, event.RecordedAtMs)
		setExecution(mutation, core.MaterializedExecutionStateIdle{})
	}
	if command == core.RelayCommandKindPrompt {
		// A prompt that never started has its acceptance ordinal on the
		// queue entry; one that started carries it on the active turn.
		outcome := &core.MaterializedTurnOutcome{
			CommandID:        commandID,
			AcceptedOrdinal:  acceptedOrdinal,
			CompletedOrdinal: event.Ordinal,
			CompletedAtMs:    event.RecordedAtMs,
		}
		if active != nil {
			start := active.TurnStartPosition
			outcome.TurnStartPosition = &start
		}
		if failure.rejected {
			outcome.Outcome = core.TurnOutcomeKindRejected{Reason: failure.reason, Message: message}
		} else {
			outcome.Outcome = core.TurnOutcomeKindInterrupted{Reason: failure.reason, Message: message}
		}
		mutation.LastTurnOutcome = outcome
		if active != nil {
			clearActiveTurn(mutation)
		}
	}
	if _, closing := current.Execution.(core.MaterializedExecutionStateClosing); command == core.RelayCommandKindClose && closing {
		setExecution(mutation, core.MaterializedExecutionStateIdle{})
	}
	switch {
	case command == core.RelayCommandKindRunUserShell:
		if existing := index.get(userShellItemID(commandID)); existing != nil {
			item := cloneTranscriptItem(existing)
			if body, ok := item.Body.(core.TranscriptBodySystem); ok {
				text := strings.Replace(body.Text, "Shell · queued", "Shell · interrupted", 1)
				text = strings.Replace(text, "Shell · running", "Shell · interrupted", 1)
				body.Text = fmt.Sprintf("%s\nerror: %s", text, message)
				item.Body = body
			}
			item.LastChangedAtMs = max(item.LastChangedAtMs, event.RecordedAtMs)
			upsert(mutation, item)
		}
	case promptWasStarted && !failure.rejected:
		pushSystemWithID(mutation, event,
			fmt.Sprintf("%s%d", core.WorkInterruptedItemPrefix, event.Ordinal),
			"Work interrupted: "+message)
	case !controllerCoordination(command) || (failure.reason != nil && !failure.reason.ExpectedCancellation()):
		// The command id is internal. The relay event keeps it for
		// diagnosis, and the log line below ties it to the notice.
		slog.Info("relay command did not complete", "command_id", commandID, "command", command, "message", message)
		pushSystem(mutation, event, message)
	}
	// Expected coordination cleanup stays in the journal and API. Typed
	// control failures remain visible to the person.
}

// projectTerminalOutput attaches a terminal's output to every tool item that
// refers to it, or parks it in a standalone item until a call does.
func projectTerminalOutput(
	index *ProjectionIndex,
	event *core.RelayEvent,
	observation *core.RelayObservationTerminalOutput,
	mutation *core.MaterializedSessionMutation,
) error {
	terminalID := observation.TerminalID
	record := core.TerminalOutputRecord{
		TerminalID: terminalID,
		Output:     core.NewPackedText(observation.Output),
		Truncated:  observation.Truncated,
		ExitCode:   observation.ExitCode,
		Signal:     observation.Signal,
	}
	rawOwner := uniquelyMatchingRawTool(index, &record)
	referrers := index.TerminalReferrers(terminalID)
	attached := false
	for _, existing := range referrers {
		if rawOwner != nil && rawOwner.StableID != existing.StableID {
			if fallback, err := fallbackToolItem(existing); err == nil && fallback {
				mutation.Transcript = append(mutation.Transcript, core.TranscriptMutationRemove{StableID: existing.StableID})
				attached = true
				continue
			}
		}
		item := cloneTranscriptItem(existing)
		body, ok := item.Body.(core.TranscriptBodyTool)
		if !ok {
			panic("matched a tool body above")
		}
		replaceOrPushTerminalRecord(&body.TerminalOutputs, record)
		if !slices.Contains(body.TerminalRefs, terminalID) {
			body.TerminalRefs = append(body.TerminalRefs, terminalID)
		}
		item.Body = body
		if err := finalizeFallbackTerminalTool(&item); err != nil {
			return err
		}
		item.LastChangedAtMs = max(item.LastChangedAtMs, event.RecordedAtMs)
		upsert(mutation, item)
		attached = true
	}
	if rawOwner != nil && !slices.ContainsFunc(referrers, func(referrer *core.TranscriptItem) bool {
		return referrer.StableID == rawOwner.StableID
	}) {
		item := cloneTranscriptItem(rawOwner)
		body, ok := item.Body.(core.TranscriptBodyTool)
		if !ok {
			panic("matched a tool body above")
		}
		replaceOrPushTerminalRecord(&body.TerminalOutputs, record)
		body.TerminalRefs = append(body.TerminalRefs, terminalID)
		item.Body = body
		item.LastChangedAtMs = max(item.LastChangedAtMs, event.RecordedAtMs)
		upsert(mutation, item)
		attached = true
	}
	if !attached {
		stableID := terminalItemID(terminalID)
		if existing := index.get(stableID); existing != nil {
			item := cloneTranscriptItem(existing)
			item.Body = core.TranscriptBodyTerminalOutputOf(record)
			item.LastChangedAtMs = max(item.LastChangedAtMs, event.RecordedAtMs)
			upsert(mutation, item)
		} else {
			upsert(mutation, core.TranscriptItem{
				StableID:        stableID,
				Position:        event.Ordinal,
				CreatedAtMs:     event.RecordedAtMs,
				LastChangedAtMs: event.RecordedAtMs,
				Body:            core.TranscriptBodyTerminalOutputOf(record),
			})
		}
	}
	return nil
}

// answeredThrough: the projection alone decides which accepted prompts a
// completion answers: every prompt accepted before it, unless one is still
// queued or running, and never fewer than the previous outcome answered.
//
// One number records this, so a prompt settled out of order (a queued
// prompt rejected or removed while an earlier one still runs or waits) is
// answered by its own outcome, and by later outcomes only once the prompts
// ahead of it have ended. A wait for it can therefore end late, but never
// early.
//
// While another turn runs, its own prompt is still owed and every earlier
// prompt settled before it started, so the outcome adds nothing. The
// running turn's ordinal is never read: steering replaces it with the
// steered prompt's (#1450 review). The answer is always recorded, 0 meaning
// none, so a reader never falls back to the outcome's own acceptance
// ordinal; the result is therefore never nil.
func answeredThrough(current *core.MaterializedSession, event *core.RelayEvent, ending string) *uint64 {
	anotherRunning := current.ActiveTurn != nil && !current.ActiveTurn.BelongsTo(ending)
	var own *uint64
	if !anotherRunning {
		var first *uint64
		for i := range current.QueuedPrompts {
			queued := &current.QueuedPrompts[i]
			// A queued configuration change answers no prompt and ends in
			// no turn outcome, so it never holds the answer back.
			if !queuedIsPrompt(queued) || queued.CommandID == ending || queued.AcceptedOrdinal == nil {
				continue
			}
			if first == nil || *queued.AcceptedOrdinal < *first {
				ordinal := *queued.AcceptedOrdinal
				first = &ordinal
			}
		}
		switch {
		case first == nil:
			ordinal := event.Ordinal
			own = &ordinal
		case *first > 0:
			capped := min(*first-1, event.Ordinal)
			own = &capped
		}
	}
	var answered uint64
	if current.LastTurnOutcome != nil {
		if previous, ok := current.LastTurnOutcome.AnsweredOrdinal(); ok {
			answered = previous
		}
	}
	if own != nil {
		answered = max(answered, *own)
	}
	return &answered
}

// externalEventsItemID is the row that lists one delivery's external
// events, such as GitHub activity. It is keyed on the delivery's first
// external event, as a message row is keyed on its event, so a replayed
// delivery finds the row it already made. The relay claims each event key
// for exactly one wake, prompt, or hook delivery (claim_wake_mailbox_events
// and the delivered-key check in mj-core's relay apply), so no two
// deliveries share a row identity. False when the delivery carried only
// messages, or carried no bodies because an older worker recorded it.
func externalEventsItemID(events []core.MailboxEvent) (string, bool) {
	for i := range events {
		if _, message := events[i].MessageText(); !message {
			return externalEventsItemPrefix + events[i].Key, true
		}
	}
	return "", false
}

// deliveryRowExists: a replayed delivery leaves the rows it made where they
// are, whether they are in the projection already or earlier in this
// mutation.
func deliveryRowExists(index *ProjectionIndex, mutation *core.MaterializedSessionMutation, stableID string) bool {
	if index.get(stableID) != nil {
		return true
	}
	for _, change := range mutation.Transcript {
		if upsert, ok := change.(core.TranscriptMutationUpsert); ok && upsert.Item.StableID == stableID {
			return true
		}
	}
	return false
}

// deliveredExternalEvents shows external events where the agent received
// them: just before the turn a wake started, before the prompt they were
// attached to, or between the tool calls a hook delivered them after.
// Messages get rows of their own in deliveredMessages; this row lists
// everything else in the delivery.
func deliveredExternalEvents(index *ProjectionIndex, delivery *core.RelayEvent, events []core.MailboxEvent, mutation *core.MaterializedSessionMutation) {
	stableID, ok := externalEventsItemID(events)
	if !ok {
		return
	}
	if deliveryRowExists(index, mutation, stableID) {
		return
	}
	var lines []string
	for i := range events {
		event := &events[i]
		if _, message := event.MessageText(); message {
			continue
		}
		if _, away := event.Body.(core.MailboxEventBodyOperatorAway); away {
			continue
		}
		description := core.DescribeMailboxEvent(event)
		// A child report is the parent's only record of the child's result,
		// so show the report the agent received, not a summary.
		if _, finished := event.Body.(core.MailboxEventBodySubagentFinished); finished && description.Body != nil {
			report := strings.Join(projectionLines(*description.Body), "\n    ")
			lines = append(lines, fmt.Sprintf("%s\n    %s", description.TranscriptLine, report))
		} else {
			lines = append(lines, description.TranscriptLine)
		}
	}
	if len(lines) == 0 {
		return
	}
	closeStreams(index, mutation, delivery.RecordedAtMs)
	pushSystemWithID(mutation, delivery, stableID, "Mailbox events:\n  - "+strings.Join(lines, "\n  - "))
}

// projectionLines is Rust's str::lines: split on newlines, drop one trailing
// carriage return per line, and no final empty line.
func projectionLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for i, line := range lines {
		lines[i] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

func deliveredMessages(index *ProjectionIndex, delivery *core.RelayEvent, events []core.MailboxEvent, mutation *core.MaterializedSessionMutation) {
	var messages []*core.MailboxEvent
	for i := range events {
		if _, message := events[i].MessageText(); message {
			messages = append(messages, &events[i])
		}
	}
	// Equal-position rows use stable identity order in the durable reader.
	// Keep the live batch in that same order across detach and restoration.
	sort.SliceStable(messages, func(left, right int) bool {
		return messages[left].Key < messages[right].Key
	})
	for _, event := range messages {
		stableID := core.MessageItemID(event.Key)
		if deliveryRowExists(index, mutation, stableID) {
			continue
		}
		closeStreams(index, mutation, delivery.RecordedAtMs)
		delivered := *event
		upsert(mutation, core.TranscriptItem{
			StableID:        stableID,
			Position:        delivery.Ordinal,
			CreatedAtMs:     delivery.RecordedAtMs,
			LastChangedAtMs: delivery.RecordedAtMs,
			Body:            core.TranscriptBodyMessage{Event: &delivered},
		})
	}
}

func userShellItemID(commandID string) string {
	return "shell:" + commandID
}

// PlanProposalItemID is the stable id of the captured plan proposal created
// by the relay event at ordinal. The ordinal keys it because the
// harness-side review id restarts with every harness process, while the
// ordinal is durable and replay-stable.
func PlanProposalItemID(ordinal uint64) string {
	return fmt.Sprintf("plan-proposal:%d", ordinal)
}

func userShellText(command, status, stdout, stderr string, stdoutTruncated, stderrTruncated bool) string {
	var text strings.Builder
	fmt.Fprintf(&text, "Shell · %s\n$ %s", status, command)
	for _, stream := range []struct {
		name, output string
		truncated    bool
	}{{"stdout", stdout, stdoutTruncated}, {"stderr", stderr, stderrTruncated}} {
		if stream.output == "" {
			continue
		}
		text.WriteString("\n\n" + stream.name + ":\n" + stream.output)
		if stream.truncated {
			text.WriteString("\n[output continues; final tail will be shown on completion]")
		}
	}
	return text.String()
}

func userShellResultText(result *core.UserShellResult) string {
	var status string
	switch result.Status {
	case core.UserShellStatusExited:
		switch {
		case result.ExitCode == nil:
			status = "finished"
		case *result.ExitCode == 0:
			status = "done"
		default:
			status = fmt.Sprintf("failed (exit %d)", *result.ExitCode)
		}
	case core.UserShellStatusSignaled:
		signal := "unknown signal"
		if result.Signal != nil {
			signal = *result.Signal
		}
		status = fmt.Sprintf("signaled (%s)", signal)
	case core.UserShellStatusTimedOut:
		status = "timed out"
	case core.UserShellStatusCancelled:
		status = "cancelled"
	case core.UserShellStatusInterrupted:
		status = "interrupted"
	case core.UserShellStatusFailed:
		status = "failed"
	}
	text := userShellText(result.Command, fmt.Sprintf("%s · %d ms", status, result.DurationMs),
		result.Stdout, result.Stderr, result.StdoutTruncated, result.StderrTruncated)
	if result.Error != nil {
		text += "\n\nerror: " + *result.Error
	}
	return text
}

// controllerCoordination reports commands the daemon issues to the worker
// for its own bookkeeping, never on the person's behalf.
func controllerCoordination(command core.RelayCommandKind) bool {
	switch command {
	case core.RelayCommandKindBeginCheckpoint, core.RelayCommandKindCompleteCheckpoint,
		core.RelayCommandKindReleaseCheckpoint, core.RelayCommandKindAdvanceRecoveryFloor:
		return true
	}
	return false
}

// Port of mj-transcript/src/projection/live.rs.

package transcript

import (
	"errors"
	"fmt"
	"maps"
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// MaterializedSessionFromLiveBaseline builds the live recovery cut without
// claiming to recover any earlier history.
func MaterializedSessionFromLiveBaseline(baseline *core.LiveSessionBaseline) (core.MaterializedSession, error) {
	state := &baseline.State
	if state.LatestOrdinal == 0 {
		return core.MaterializedSession{}, errors.New("worker has not recorded its startup event; wait for worker startup to complete, then adopt")
	}
	projection := core.MaterializedSessionEmpty(state.SessionID)
	projection.AppliedEventOrdinal = state.LatestOrdinal
	projection.AppliedEventDigest = state.LatestDigest
	values := make(map[string]any, len(state.Config))
	for key, value := range state.Config {
		values[key] = value
	}
	optionValues, err := ConfigurationValues(state.ConfigOptions)
	if err != nil {
		return core.MaterializedSession{}, err
	}
	maps.Copy(values, optionValues)
	for _, canonical := range []string{"model", "effort"} {
		option := core.FindSessionConfigOption(state.ConfigOptions, canonical)
		if option == nil {
			continue
		}
		value, ok := values[string(option.ID)]
		if !ok {
			continue
		}
		// A provider restart may select a replacement for a withdrawn
		// model. Only an unselectable transcript ID needs the previously
		// accepted picker value instead of the catalogue's current value.
		current, isString := value.(string)
		selectable := isString && core.SelectContains(option, current)
		if canonical == "model" && !selectable {
			if accepted := core.AcceptedSessionConfigFromConfiguration(state.Config, state.ConfigOptions).Model; accepted != nil {
				value = *accepted
			}
		}
		values[string(option.ID)] = value
		values[canonical] = value
	}
	// CurrentModeUpdate changes the legacy mode without reissuing the option
	// catalogue. Its accepted value is newer than that catalogue's mode value.
	if mode, ok := state.Config["mode"]; ok {
		values["mode"] = mode
	} else if state.Modes != nil {
		values["mode"] = string(state.Modes.CurrentModeID)
	}
	delete(values, core.ProjectionKey)
	delete(values, HandoffStateKey)
	projection.Configuration.Values = values
	if state.Goal.Known {
		goal := state.Goal
		projection.Configuration.Goal = &goal
	}
	now := core.EpochMillis()
	projection.LastActivityAtMs = &now
	projection.Execution = core.MaterializedExecutionStateIdle{}
	projection.PendingElicitations = slices.Clone(baseline.Questions)
	for i := range baseline.Queue {
		queued := &baseline.Queue[i]
		var kind core.QueuedCommandKind = core.QueuedCommandKindPrompt{}
		if config, ok := queued.Command.(core.RelayCommandSetConfig); ok {
			kind = core.QueuedCommandKindSetConfig{Key: config.Key, Value: config.Value}
		}
		content, err := QueuedCommandContent(queued.Command)
		if err != nil {
			return core.MaterializedSession{}, err
		}
		projection.QueuedPrompts = append(projection.QueuedPrompts, core.MaterializedQueuedPrompt{
			CommandID:       queued.CommandID,
			Kind:            kind,
			Content:         queuedContentValues(content),
			QueuedAtMs:      queued.CreatedAtMs,
			AcceptedOrdinal: queued.AcceptedOrdinal,
		})
	}
	beforeLatest := max(state.LatestOrdinal-1, 1)
	for i := range baseline.DeliveredInitialPrompts {
		initial := &baseline.DeliveredInitialPrompts[i]
		content, err := QueuedCommandContent(initial.Command)
		if err != nil {
			return core.MaterializedSession{}, err
		}
		projection.Transcript = append(projection.Transcript, &core.TranscriptItem{
			StableID:        "user:" + initial.CommandID,
			Position:        beforeLatest,
			CreatedAtMs:     initial.CreatedAtMs,
			LastChangedAtMs: initial.CreatedAtMs,
			Body:            core.TranscriptBodyUser{Content: content},
		})
	}
	for i := range state.ActiveUserShells {
		shell := &state.ActiveUserShells[i]
		if shell.StartedAtMs != nil {
			continue
		}
		projection.Transcript = append(projection.Transcript, &core.TranscriptItem{
			StableID:        userShellItemID(shell.CommandID),
			Position:        beforeLatest,
			CreatedAtMs:     shell.CreatedAtMs,
			LastChangedAtMs: shell.CreatedAtMs,
			Body:            core.TranscriptBodySystem{Text: userShellText(shell.Command, "queued", "", "", false, false)},
		})
	}
	projection.Transcript = append(projection.Transcript, &core.TranscriptItem{
		StableID:        core.AdoptionHistoryUnavailableItemID,
		Position:        state.LatestOrdinal,
		CreatedAtMs:     now,
		LastChangedAtMs: now,
		Body: core.TranscriptBodySystem{Text: fmt.Sprintf(
			"History before recovery unavailable. Earlier transcript, including events through %d, was not recovered; the agent's own conversation is intact.",
			state.LatestOrdinal)},
	})
	if baseline.UnavailableQuestions > 0 {
		projection.Transcript = append(projection.Transcript, &core.TranscriptItem{
			StableID:        "system:adoption-questions-unavailable",
			Position:        state.LatestOrdinal,
			CreatedAtMs:     now,
			LastChangedAtMs: now,
			Body: core.TranscriptBodySystem{
				Text: "An earlier pending question could not be recovered because its saved history was discarded. Ask the agent to repeat it.",
			},
		})
	}
	if err := projection.Validate(); err != nil {
		return core.MaterializedSession{}, err
	}
	return projection, nil
}

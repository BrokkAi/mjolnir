// Port of mj-worker/src/acp/verdict_client/glide_turn.rs.

// Evidence-only GLiDE boundary, independent of the older classifiers'
// budgets.

package worker

import (
	"errors"
	"fmt"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// project is the GLiDE completed-turn request body (a JSON value) for
// evidence, judged against history.
func project(evidence *core.TurnEvidence, history core.ContextHistory, facts *core.ActivityFacts, nativeAgentCount int) (any, error) {
	if evidence.Phase != core.TurnPhaseReplied {
		return nil, errors.New("GLiDE requires a completed turn")
	}
	base := *evidence
	base.TranscriptSummary = ""
	authorization := history
	base.Authorization = &authorization
	parsed, err := jsonvalue.Parse(core.RequestBody(base, nil))
	if err != nil {
		return nil, err
	}
	body, ok := parsed.(jsonvalue.Object)
	if !ok {
		return nil, errors.New("GLiDE evidence is not an object")
	}
	latest := -1
	for index := len(history.Messages) - 1; index >= 0; index-- {
		if history.Messages[index].Role == "user" {
			latest = index
			break
		}
	}
	latestText := ""
	if latest >= 0 {
		latestText = history.Messages[latest].Text
	}
	body.Set("latest_user_message", latestText)
	replies := []any{}
	for _, message := range history.Messages[latest+1:] {
		if message.Role == "assistant" {
			replies = append(replies, message.Text)
		}
	}
	body.Set("current_assistant_replies", replies)
	toolsInFlight, err := glideTurnJSONList(base.ToolsInFlight)
	if err != nil {
		return nil, err
	}
	finalToolCalls, err := glideTurnJSONList(base.FinalToolCalls)
	if err != nil {
		return nil, err
	}
	background, err := glideTurnJSONList(base.Background)
	if err != nil {
		return nil, err
	}
	body.Set("runtime_observations", []any{
		fmt.Sprintf("Current phase %s, silence %d seconds.", verdictDebugVariant(string(base.Phase)), base.SilentForS),
		fmt.Sprintf("Currently observed tools: %s; background commands: %d; queued prompts: %d.",
			toolsInFlight, facts.BackgroundCommands, facts.QueuedCommands),
		fmt.Sprintf("Final tool outcomes: %s; background inventory: %s.", finalToolCalls, background),
		fmt.Sprintf("Currently running native agents: %d. Goal active: %t; running: %t; pending resume: %t.",
			nativeAgentCount, facts.GoalActive, facts.GoalRunning, facts.GoalPendingResume),
	})
	var redacted any = body
	core.RedactJSONStrings(&redacted)
	return redacted, nil
}

// glideTurnJSONList is serde_json::to_string of a Vec: `[]` when empty.
func glideTurnJSONList[T any](items []T) (string, error) {
	if items == nil {
		items = []T{}
	}
	encoded, err := acpFeaturesMarshalJSON(items)
	if err != nil {
		return "", err
	}
	return string(encoded), nil
}

func shrink(history *core.ContextHistory) bool {
	latestUser := -1
	for index := len(history.Messages) - 1; index >= 0; index-- {
		if history.Messages[index].Role == "user" {
			latestUser = index
			break
		}
	}
	if latestUser > 0 {
		removed := history.Messages[0]
		history.Messages = append(history.Messages[:0:0], history.Messages[1:]...)
		if removed.Role == "user" {
			history.AuthorizationComplete = false
		} else {
			history.AssistantHistoryOmitted = true
		}
		return true
	}
	// Iterator::max_by_key: the last of equally long messages.
	index := -1
	for candidate := range history.Messages {
		if index < 0 || len(history.Messages[candidate].Text) >= len(history.Messages[index].Text) {
			index = candidate
		}
	}
	if index < 0 {
		return false
	}
	text := history.Messages[index].Text
	if text == "" {
		return false
	}
	keep := len(text) / 2
	head := keep / 2
	tail := keep - head
	head = glideTurnFloorCharBoundary(text, head)
	tailStart := glideTurnCeilCharBoundary(text, len(text)-tail)
	messages := append(history.Messages[:0:0], history.Messages...)
	messages[index].Text = text[:head] + text[tailStart:]
	history.Messages = messages
	if messages[index].Role == "user" {
		history.AuthorizationComplete = false
	} else {
		history.AssistantHistoryOmitted = true
	}
	return true
}

// glideTurnFloorCharBoundary is str::floor_char_boundary.
func glideTurnFloorCharBoundary(text string, index int) int {
	if index >= len(text) {
		return len(text)
	}
	for index > 0 && !glideTurnIsCharBoundary(text, index) {
		index--
	}
	return index
}

// glideTurnCeilCharBoundary is str::ceil_char_boundary.
func glideTurnCeilCharBoundary(text string, index int) int {
	if index > len(text) {
		return len(text)
	}
	for index < len(text) && !glideTurnIsCharBoundary(text, index) {
		index++
	}
	return index
}

func glideTurnIsCharBoundary(text string, index int) bool {
	return index == 0 || index == len(text) || text[index]&0xC0 != 0x80
}

// Probabilities is GLiDE's completed-turn answer (Rust re-exports
// mj_core::activity::eot_glide::types::Probabilities); parse it with
// core.ProbabilitiesParse.
type Probabilities = core.Probabilities

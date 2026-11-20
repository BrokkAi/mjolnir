// Port of mj-core/src/acp/dialect/grok.rs (and the doc of acp/dialect/mod.rs).

// Compatibility policy for ACP agents that deviate from the standard wire
// surface: Grok Build's legacy ACP extensions.

package core

import (
	"context"
	"encoding/json"
	"fmt"
	"slices"
	"strings"

	acp "github.com/BrokkAi/acp-go"
	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/BrokkAi/acp-go/schema"
)

const grokExitPlanModeMethod = "x.ai/exit_plan_mode"
const grokSetModelMethod = "session/set_model"
const grokPlanReviewIDPrefix = "plan-review-grok-"

type GrokModelState struct {
	currentModelID string
	currentEffort  *string
	models         []grokModel
}

type grokModel struct {
	id            string
	name          string
	description   *string
	defaultEffort *string
	efforts       []GrokChoice
}

type GrokChoice struct {
	ID          string
	name        string
	description *string
}

func (s *GrokModelState) currentModel() *grokModel {
	for i := range s.models {
		if s.models[i].id == s.currentModelID {
			return &s.models[i]
		}
	}
	return nil
}

func grokIsExitPlanModeMethod(method string) bool {
	if len(method) > 0 && method[0] == '_' {
		method = method[1:]
	}
	return method == grokExitPlanModeMethod
}

func GrokHandlesExitPlanMode(harness HarnessKind, method string) bool {
	return harness == HarnessKindGrok && grokIsExitPlanModeMethod(method)
}

func GrokPlanReviewID(sequence uint64) string {
	return fmt.Sprintf("%s%d", grokPlanReviewIDPrefix, sequence)
}

func GrokIsPlanReviewID(id string) bool {
	return len(id) >= len(grokPlanReviewIDPrefix) && id[:len(grokPlanReviewIDPrefix)] == grokPlanReviewIDPrefix
}

// GrokPlanResponse is the JSON result Grok expects for an exit-plan-mode
// request, from the person's plan review answer.
func GrokPlanResponse(response ElicitationResponse) json.RawMessage {
	action, feedback := PlanReviewAnswer(response)
	var value any
	switch action {
	case "implement":
		value = map[string]any{"outcome": "approved"}
	case "exit":
		value = map[string]any{"outcome": "abandoned"}
	case "revise":
		if feedback != nil {
			value = jsonvalue.Object{{Key: "outcome", Value: "cancelled"}, {Key: "feedback", Value: *feedback}}
		} else {
			value = map[string]any{"outcome": "cancelled"}
		}
	default:
		value = map[string]any{"outcome": "cancelled"}
	}
	encoded, err := agentMarshalJSON(value)
	if err != nil {
		panic(fmt.Sprintf("encode Grok plan response: %v", err))
	}
	return encoded
}

// GrokModelStateOf reads Grok's model catalogue from ACP initialization or
// session metadata. (Rust: `grok::model_state`.)
func GrokModelStateOf(meta schema.Meta) *GrokModelState {
	if meta == nil {
		return nil
	}
	state, ok := meta.Get("modelState")
	if !ok {
		return nil
	}
	return grokModelStateFromValue(state)
}

func grokModelStateFromValue(state any) *GrokModelState {
	currentModelID, ok := agentJSONString(state, "currentModelId")
	if !ok {
		return nil
	}
	available, _ := agentJSONGet(state, "availableModels")
	entries, ok := available.([]any)
	if !ok {
		return nil
	}
	var models []grokModel
	for _, model := range entries {
		id, ok := agentJSONString(model, "modelId")
		if !ok {
			continue
		}
		var efforts []GrokChoice
		if listed, ok := agentJSONPointer(model, "_meta", "reasoningEfforts"); ok {
			if listed, ok := listed.([]any); ok {
				for _, effort := range listed {
					effortValue, found := agentJSONGet(effort, "value")
					if !found {
						effortValue, _ = agentJSONGet(effort, "id")
					}
					effortID, ok := effortValue.(string)
					if !ok {
						continue
					}
					name, ok := agentJSONString(effort, "label")
					if !ok {
						name = effortID
					}
					var description *string
					if text, ok := agentJSONString(effort, "description"); ok {
						description = &text
					}
					efforts = append(efforts, GrokChoice{ID: effortID, name: name, description: description})
				}
			}
		}
		name, ok := agentJSONString(model, "name")
		if !ok {
			name = id
		}
		var description *string
		if text, ok := agentJSONString(model, "description"); ok {
			description = &text
		}
		var defaultEffort *string
		if effort, ok := agentJSONPointer(model, "_meta", "reasoningEffort"); ok {
			if text, ok := effort.(string); ok {
				defaultEffort = &text
			}
		}
		models = append(models, grokModel{
			id:            id,
			name:          name,
			description:   description,
			defaultEffort: defaultEffort,
			efforts:       efforts,
		})
	}
	if len(models) == 0 {
		return nil
	}
	parsed := &GrokModelState{currentModelID: currentModelID, models: models}
	if current := parsed.currentModel(); current != nil && current.defaultEffort != nil {
		effort := *current.defaultEffort
		parsed.currentEffort = &effort
	}
	return parsed
}

// GrokConfigOptions presents Grok's catalogue as the standard selectors
// consumed by Hel.
func GrokConfigOptions(state *GrokModelState) []schema.SessionConfigOption {
	choice := func(choice GrokChoice) schema.SessionConfigSelectOption {
		return schema.SessionConfigSelectOption{
			Value:       schema.SessionConfigValueId(choice.ID),
			Name:        choice.name,
			Description: choice.description,
		}
	}
	models := make([]schema.SessionConfigSelectOption, 0, len(state.models))
	for _, model := range state.models {
		models = append(models, choice(GrokChoice{ID: model.id, name: model.name, description: model.description}))
	}
	modelCategory := schema.SessionConfigOptionCategoryModel
	options := []schema.SessionConfigOption{{
		Kind:     schema.SessionConfigOptionKindSelect,
		ID:       "model",
		Name:     "Model",
		Category: &modelCategory,
		Select: &schema.SessionConfigSelect{
			CurrentValue: schema.SessionConfigValueId(state.currentModelID),
			Options:      models,
		},
	}}
	var efforts []GrokChoice
	if model := state.currentModel(); model != nil {
		efforts = model.efforts
	}
	if state.currentEffort != nil && len(efforts) > 0 {
		choices := make([]schema.SessionConfigSelectOption, 0, len(efforts))
		for _, effort := range efforts {
			choices = append(choices, choice(effort))
		}
		effortCategory := schema.SessionConfigOptionCategoryThoughtLevel
		options = append(options, schema.SessionConfigOption{
			Kind:     schema.SessionConfigOptionKindSelect,
			ID:       "effort",
			Name:     "Reasoning effort",
			Category: &effortCategory,
			Select: &schema.SessionConfigSelect{
				CurrentValue: schema.SessionConfigValueId(*state.currentEffort),
				Options:      choices,
			},
		})
	}
	return options
}

func GrokMergeConfigOptions(options *[]schema.SessionConfigOption, state *GrokModelState) {
	for _, synthesized := range GrokConfigOptions(state) {
		replaced := false
		for i := range *options {
			if (*options)[i].ID == synthesized.ID {
				(*options)[i] = synthesized
				replaced = true
				break
			}
		}
		if !replaced {
			*options = append(*options, synthesized)
		}
	}
}

func GrokHandlesConfigKey(key string) bool {
	return key == "model" || key == "effort"
}

// GrokSetModelRequest builds the `session/set_model` params for a selector
// change, and the model state that holds once Grok accepts it.
func GrokSetModelRequest(sessionID schema.SessionId, state *GrokModelState, key, value string) (json.RawMessage, GrokModelState, error) {
	updated := *state
	var modelID string
	switch key {
	case "model":
		var chosen *grokModel
		for index := range state.models {
			if state.models[index].id == value {
				chosen = &state.models[index]
				break
			}
		}
		if chosen == nil {
			return nil, GrokModelState{}, fmt.Errorf("%s is not an available model value", debugString(value))
		}
		updated.currentModelID = value
		updated.currentEffort = nil
		if chosen.defaultEffort != nil {
			effort := *chosen.defaultEffort
			updated.currentEffort = &effort
		}
		modelID = value
	case "effort":
		current := state.currentModel()
		if current == nil || !slices.ContainsFunc(current.efforts, func(effort GrokChoice) bool { return effort.ID == value }) {
			return nil, GrokModelState{}, fmt.Errorf("%s is not an available effort value", debugString(value))
		}
		effort := value
		updated.currentEffort = &effort
		modelID = state.currentModelID
	default:
		return nil, GrokModelState{}, fmt.Errorf("Grok Build has no %s selector", key)
	}
	params := jsonvalue.Object{
		{Key: "sessionId", Value: string(sessionID)},
		{Key: "modelId", Value: modelID},
	}
	if key == "effort" {
		params = append(params, jsonvalue.Member{Key: "_meta", Value: jsonvalue.Object{{Key: "reasoningEffort", Value: value}}})
	}
	encoded, err := params.MarshalJSON()
	if err != nil {
		return nil, GrokModelState{}, err
	}
	return encoded, updated, nil
}

func GrokApplyModelChange(ctx context.Context, connection *acp.Connection, sessionID schema.SessionId, state *GrokModelState, key, value string) error {
	params, updated, err := GrokSetModelRequest(sessionID, state, key, value)
	if err != nil {
		return err
	}
	if err := connection.Call(ctx, grokSetModelMethod, params, nil); err != nil {
		return fmt.Errorf("set session %s to %s: %w", key, value, err)
	}
	*state = updated
	return nil
}

func GrokResponseWasLost(err error) bool {
	for _, cause := range agentErrorChain(err) {
		rpc, ok := cause.(*acp.RPCError)
		if ok && rpc.Code == -32603 &&
			strings.HasPrefix(rpc.Message, "response to `session/set_model` never received:") {
			return true
		}
	}
	return false
}

func GrokPermitsUnadvertisedPlanMode(harness HarnessKind, modeID string) bool {
	return harness == HarnessKindGrok && (modeID == "plan" || modeID == "default")
}

// Port of mj-core/src/acp/dialect/grok.rs (and the doc of acp/dialect/mod.rs).

// Compatibility policy for ACP agents that deviate from the standard wire
// surface: Grok Build's legacy ACP extensions.

package core

import (
	"context"
	"encoding/json"
	"fmt"

	acp "github.com/BrokkAi/acp-go"
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
	panic("unported: mj-core/src/acp/dialect/grok.rs plan_response")
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
	panic("unported: mj-core/src/acp/dialect/grok.rs model_state_from_value")
}

// GrokConfigOptions presents Grok's catalogue as the standard selectors
// consumed by Hel.
func GrokConfigOptions(state *GrokModelState) []schema.SessionConfigOption {
	panic("unported: mj-core/src/acp/dialect/grok.rs config_options")
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
	panic("unported: mj-core/src/acp/dialect/grok.rs set_model_request")
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
	panic("unported: mj-core/src/acp/dialect/grok.rs response_was_lost")
}

func GrokPermitsUnadvertisedPlanMode(harness HarnessKind, modeID string) bool {
	return harness == HarnessKindGrok && (modeID == "plan" || modeID == "default")
}

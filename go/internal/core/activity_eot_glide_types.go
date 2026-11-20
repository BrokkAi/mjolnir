// Port of mj-core/src/activity/eot_glide/types.rs.

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"slices"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

type Distribution[T any] struct {
	Probabilities T `json:"probabilities"`
}

type Probabilities struct {
	Attention      *Distribution[Attention]     `json:"attention,omitempty"`
	Input          Distribution[Input]          `json:"input"`
	Failure        Distribution[Failure]        `json:"failure"`
	Work           Distribution[Work]           `json:"work"`
	Background     Distribution[Background]     `json:"background"`
	TurnState      Distribution[TurnState]      `json:"turn_state"`
	PermissionVeto Distribution[PermissionVeto] `json:"permission_veto"`
}

type Input struct {
	None             float64 `json:"none"`
	RedundantRequest float64 `json:"redundant_request"`
	Required         float64 `json:"required"`
	Unclear          float64 `json:"unclear"`
}

type Failure struct {
	None              float64 `json:"none"`
	TransientProvider float64 `json:"transient_provider"`
	Quota             float64 `json:"quota"`
	Other             float64 `json:"other"`
	Unclear           float64 `json:"unclear"`
}

type Work struct {
	Finished             float64 `json:"finished"`
	AuthorizedUnfinished float64 `json:"authorized_unfinished"`
	Waiting              float64 `json:"waiting"`
	Unclear              float64 `json:"unclear"`
}

type Background struct {
	Needed   float64 `json:"needed"`
	Unneeded float64 `json:"unneeded"`
	Unclear  float64 `json:"unclear"`
}

type TurnState struct {
	RecoverQuota  float64 `json:"recover_quota"`
	AwaitInput    float64 `json:"await_input"`
	RetryProvider float64 `json:"retry_provider"`
	Wait          float64 `json:"wait"`
	ContinueWork  float64 `json:"continue"`
	Finished      float64 `json:"finished"`
	Unclear       float64 `json:"unclear"`
}

type PermissionVeto struct {
	RedundantOnly    float64 `json:"redundant_only"`
	NotRedundantOnly float64 `json:"not_redundant_only"`
	NoRequest        float64 `json:"no_request"`
	Unknown          float64 `json:"unknown"`
}

type Attention struct {
	NeedsInput       float64 `json:"needs_input"`
	Important        float64 `json:"important"`
	RedundantRequest float64 `json:"redundant_request"`
	None             float64 `json:"none"`
	Unclear          float64 `json:"unclear"`
}

// deny_unknown_fields on every distribution.

func (v *Input) UnmarshalJSON(data []byte) error {
	type plain Input
	return agentDecodeStrict(data, (*plain)(v))
}

func (v *Failure) UnmarshalJSON(data []byte) error {
	type plain Failure
	return agentDecodeStrict(data, (*plain)(v))
}

func (v *Work) UnmarshalJSON(data []byte) error {
	type plain Work
	return agentDecodeStrict(data, (*plain)(v))
}

func (v *Background) UnmarshalJSON(data []byte) error {
	type plain Background
	return agentDecodeStrict(data, (*plain)(v))
}

func (v *TurnState) UnmarshalJSON(data []byte) error {
	type plain TurnState
	return agentDecodeStrict(data, (*plain)(v))
}

func (v *PermissionVeto) UnmarshalJSON(data []byte) error {
	type plain PermissionVeto
	return agentDecodeStrict(data, (*plain)(v))
}

func (v *Attention) UnmarshalJSON(data []byte) error {
	type plain Attention
	return agentDecodeStrict(data, (*plain)(v))
}

// optionalAxes are questions added after the frozen set. The hosted proxy and
// the workers upgrade independently, so a proxy that predates one of these
// omits its answer. That question then has no effect, instead of every
// verdict failing. An answer that is present must still be complete and
// valid.
var optionalAxes = []string{"attention"}

// ProbabilitiesParse validates a GLiDE response's answers against the
// bundled questions and decodes them.
func ProbabilitiesParse(response any) (Probabilities, error) {
	answers, ok := agentJSONGet(response, "answers")
	if !ok {
		return Probabilities{}, errors.New("missing GLiDE answers")
	}
	parsed, err := jsonvalue.Parse([]byte(Questions))
	if err != nil {
		return Probabilities{}, err
	}
	questions, _ := agentJSONMembers(parsed)
	for _, question := range questions {
		axis := question.Key
		if _, present := agentJSONGet(answers, axis); slices.Contains(optionalAxes, axis) && !present {
			continue
		}
		distribution, _ := agentJSONPointer(answers, axis, "probabilities")
		scores, isObject := agentJSONMembers(distribution)
		if !isObject {
			return Probabilities{}, errors.New("missing GLiDE distribution")
		}
		criteriaValue, _ := agentJSONGet(question.Value, "criteria")
		criteria, _ := agentJSONMembers(criteriaValue)
		valid := len(scores) == len(criteria)
		for _, criterion := range criteria {
			if !valid {
				break
			}
			score, present := agentJSONGet(distribution, criterion.Key)
			probability, isNumber := agentJSONFloat(score)
			valid = present && isNumber && !math.IsInf(probability, 0) && !math.IsNaN(probability) &&
				probability >= 0 && probability <= 1
		}
		if !valid {
			return Probabilities{}, fmt.Errorf("invalid GLiDE distribution for %s", axis)
		}
		var sum float64
		for _, score := range scores {
			probability, _ := agentJSONFloat(score.Value)
			sum += probability
		}
		if math.Abs(sum-1.0) > 0.03 {
			return Probabilities{}, fmt.Errorf("unnormalized GLiDE distribution for %s", axis)
		}
	}
	data, err := jsonvalue.Marshal(answers)
	if err != nil {
		return Probabilities{}, err
	}
	var probabilities Probabilities
	if err := json.Unmarshal(data, &probabilities); err != nil {
		return Probabilities{}, err
	}
	return probabilities, nil
}

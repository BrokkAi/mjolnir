// Port of mj-core/src/activity/eot_glide/types.rs.

package core

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
	panic("unported: mj-core/src/activity/eot_glide/types.rs Probabilities::parse")
}

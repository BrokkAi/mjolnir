// Port of mj-core/src/activity/eot_glide.rs.

// Frozen GLiDE completed-turn decision rule (2026-11-12 fresh evaluation).

package core

import (
	_ "embed"
	"encoding/json"
)

// Questions is the frozen GLiDE turn question set.
//
//go:embed data/activity/eot_glide_questions.json
var Questions string

// FROZEN/rule.json, glide-eot-research-20261112-v4, fresh development fit,
// except the input cut, the demote ceiling and the attention question below.

// RequiredInput is the needs-input cut. The user's rules (2026-11-12): a 3%
// false-alert budget, and a redundant permission ask counts as needing the
// user (automatic continuation was removed, #1532). Fitted on the input
// study's tuning sessions for 2% false ? without agent-driven sessions
// (/mnt/optane/tmp/eot-input-study-v2-20261112). With the demote rule: needs
// input shown ? 92.1% (cross-fit 90.7%, held-out 96.9%), false ? 1.9%. It
// sits one float above a negative scoring 0.15844652713727192.
const RequiredInput = 0.15844652713727195

// DemoteRequiredCeiling: below this needs-input probability, a reply that is
// Important shows ! (and clears on viewing) instead of ?. At the cut above it
// moves 27 of 37 advice turns off a sticky ? and 8 of 414 real questions to
// !; .5 and .6 are indistinguishable on held-out sessions, and above .6 real
// questions demote.
const DemoteRequiredCeiling = 0.5

// ImportantAttention is the Important cut, fitted on the input study's
// corrected mask for about 3% false importants: 93.2% important recall,
// 2.98% false importants; session cross-fit 93.4% / 3.3%
// (/mnt/optane/tmp/important-calibration-20261112). A live check confirmed
// that adding the attention question changes no choices on the other
// questions.
const ImportantAttention = 0.488819272011068
const FailureFloor = 0.90
const FinishedCategorical = 0.9162209700222781
const WaitingWorkWinner = 0.40
const SafeInputNone = 0.85
const BackgroundFloor = 0.90

// Action applies the frozen rule to validated provider answers (a decoded
// JSON value). Missing probabilities abstain; callers must validate
// distributions at the provider boundary.
func Action(answers any) AssessmentAction {
	p := func(axis, name string) float64 {
		value, _ := agentJSONPointer(answers, axis, "probabilities", name)
		number, _ := agentJSONFloat(value)
		return number
	}
	if p("failure", "quota") >= FailureFloor {
		return AssessmentActionRecoverQuota
	}
	required := p("input", "required")
	// A reply that delivers requested advice with only a weak question signal
	// is Important (cleared on viewing), not needs input (cleared only by an
	// answer). A missing attention answer reads as 0 and never demotes.
	demoted := required < DemoteRequiredCeiling && p("attention", "important") >= ImportantAttention
	if required >= RequiredInput && !demoted {
		return AssessmentActionAwaitInput
	}
	if p("failure", "transient_provider") >= FailureFloor {
		return AssessmentActionRetryProvider
	}
	if p("failure", "other") >= FailureFloor {
		return AssessmentActionAwaitInput
	}
	safe := p("failure", "none") >= FailureFloor
	// The frozen rule once chose Continue here. Automatic continuation was
	// removed (#1532) and the branch with it: whenever its condition held,
	// `turn_state.continue` >= .90 left `finished` below FinishedCategorical
	// and `work.authorized_unfinished` >= .90 left `waiting` below
	// WaitingWorkWinner, so the rule already fell through to Uncertain.
	if safe && p("input", "none") >= SafeInputNone {
		if p("turn_state", "finished") >= FinishedCategorical {
			return AssessmentActionFinished
		}
		waiting := p("work", "waiting")
		if waiting >= WaitingWorkWinner &&
			waiting >= p("work", "authorized_unfinished") &&
			waiting >= p("work", "finished") &&
			waiting >= p("work", "unclear") {
			return AssessmentActionWait
		}
	}
	return AssessmentActionUncertain
}

// agentJSONFloat is serde_json's Value::as_f64: any JSON number, as a
// float64.
func agentJSONFloat(value any) (float64, bool) {
	switch value := value.(type) {
	case float64:
		return value, true
	case int64:
		return float64(value), true
	case uint64:
		return float64(value), true
	case int:
		return float64(value), true
	case json.Number:
		number, err := value.Float64()
		return number, err == nil
	}
	return 0, false
}

// BackgroundNeeded reads the background judgment out of validated provider
// answers; false when the session holds no background work.
func BackgroundNeeded(answers any, hasBackground bool) *bool {
	if !hasBackground {
		return nil
	}
	p := func(name string) float64 {
		value, _ := agentJSONPointer(answers, "background", "probabilities", name)
		number, _ := agentJSONFloat(value)
		return number
	}
	verdict := func(needed bool) *bool { return &needed }
	if p("needed") >= BackgroundFloor {
		return verdict(true)
	}
	if p("unneeded") >= BackgroundFloor {
		return verdict(false)
	}
	return nil
}

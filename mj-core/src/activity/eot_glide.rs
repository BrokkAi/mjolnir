//! Frozen GLiDE completed-turn decision rule (2026-11-12 fresh evaluation).
pub mod types;
use crate::assessment::Action;
use serde_json::Value;

pub const QUESTIONS: &str = include_str!("eot_glide_questions.json");
// FROZEN/rule.json, glide-eot-research-20261112-v4, fresh development fit,
// except the input cut, the demote ceiling and the attention question below.
/// The needs-input cut. The user's rules (2026-11-12): a 3% false-alert
/// budget, and a redundant permission ask counts as needing the user (automatic
/// continuation was removed, #1532). Fitted on the
/// input study's tuning sessions for 2% false ? without agent-driven sessions
/// (/mnt/optane/tmp/eot-input-study-v2-20261112). With the demote rule:
/// needs input shown ? 92.1% (cross-fit 90.7%, held-out 96.9%), false ?
/// 1.9%. It sits one float above a negative scoring 0.15844652713727192.
pub const REQUIRED_INPUT: f64 = 0.15844652713727195;
/// Below this needs-input probability, a reply that is Important shows ! (and
/// clears on viewing) instead of ?. At the cut above it moves 27 of 37 advice
/// turns off a sticky ? and 8 of 414 real questions to !; .5 and .6 are
/// indistinguishable on held-out sessions, and above .6 real questions demote.
pub const DEMOTE_REQUIRED_CEILING: f64 = 0.5;
/// The Important cut, fitted on the input study's corrected mask for about 3%
/// false importants: 93.2% important recall, 2.98% false importants; session
/// cross-fit 93.4% / 3.3% (/mnt/optane/tmp/important-calibration-20261112).
/// A live check confirmed that adding the attention question changes no
/// choices on the other questions.
pub const IMPORTANT_ATTENTION: f64 = 0.488819272011068;
pub const FAILURE_FLOOR: f64 = 0.90;
pub const FINISHED_CATEGORICAL: f64 = 0.9162209700222781;
pub const WAITING_WORK_WINNER: f64 = 0.40;
pub const SAFE_INPUT_NONE: f64 = 0.85;
pub const BACKGROUND_FLOOR: f64 = 0.90;

/// Apply the frozen rule to validated provider answers. Missing probabilities
/// abstain; callers must validate distributions at the provider boundary.
pub fn action(answers: &Value) -> Action {
    let p = |axis: &str, name: &str| answers[axis]["probabilities"][name].as_f64().unwrap_or(0.0);
    if p("failure", "quota") >= FAILURE_FLOOR {
        return Action::RecoverQuota;
    }
    let required = p("input", "required");
    // A reply that delivers requested advice with only a weak question signal
    // is Important (cleared on viewing), not needs input (cleared only by an
    // answer). A missing attention answer reads as 0 and never demotes.
    let demoted =
        required < DEMOTE_REQUIRED_CEILING && p("attention", "important") >= IMPORTANT_ATTENTION;
    if required >= REQUIRED_INPUT && !demoted {
        return Action::AwaitInput;
    }
    if p("failure", "transient_provider") >= FAILURE_FLOOR {
        return Action::RetryProvider;
    }
    if p("failure", "other") >= FAILURE_FLOOR {
        return Action::AwaitInput;
    }
    let safe = p("failure", "none") >= FAILURE_FLOOR;
    // The frozen rule once chose Continue here. Automatic continuation was
    // removed (#1532) and the branch with it: whenever its condition held,
    // `turn_state.continue` >= .90 left `finished` below FINISHED_CATEGORICAL
    // and `work.authorized_unfinished` >= .90 left `waiting` below
    // WAITING_WORK_WINNER, so the rule already fell through to Uncertain.
    if safe && p("input", "none") >= SAFE_INPUT_NONE {
        if p("turn_state", "finished") >= FINISHED_CATEGORICAL {
            return Action::Finished;
        }
        let waiting = p("work", "waiting");
        if waiting >= WAITING_WORK_WINNER
            && ["authorized_unfinished", "finished", "unclear"]
                .iter()
                .all(|k| waiting >= p("work", k))
        {
            return Action::Wait;
        }
    }
    Action::Uncertain
}

pub fn background_needed(answers: &Value, has_background: bool) -> Option<bool> {
    if !has_background {
        return None;
    }
    let p = |k: &str| {
        answers["background"]["probabilities"][k]
            .as_f64()
            .unwrap_or(0.0)
    };
    if p("needed") >= BACKGROUND_FLOOR {
        Some(true)
    } else if p("unneeded") >= BACKGROUND_FLOOR {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frozen_fresh_test_probabilities_match_research() {
        let cases: Value =
            serde_json::from_str(include_str!("eot_glide_fresh_sample.json")).unwrap();
        for case in cases.as_array().unwrap() {
            let expected: Action = serde_json::from_value(case["expected"].clone()).unwrap();
            assert_eq!(action(&case["answers"]), expected, "{}", case["index"]);
            assert_eq!(
                background_needed(&case["answers"], case["has_background"].as_bool().unwrap()),
                serde_json::from_value::<Option<bool>>(case["background_needed"].clone()).unwrap()
            );
        }
    }

    #[test]
    fn weak_questions_on_requested_advice_demote_only_with_an_attention_answer() {
        let cases: Value =
            serde_json::from_str(include_str!("eot_glide_fresh_sample.json")).unwrap();
        // E0028 is a recorded needs-input turn; move its question signal between
        // the cut and the demote ceiling.
        let mut answers = cases
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["index"] == "E0028")
            .unwrap()["answers"]
            .clone();
        answers["input"]["probabilities"]["required"] = 0.3.into();
        // A proxy that predates the attention question must not demote.
        assert_eq!(action(&answers), Action::AwaitInput);
        answers["attention"] = serde_json::json!({"probabilities": {
            "needs_input": 0.1, "important": 0.8, "redundant_request": 0.0,
            "none": 0.1, "unclear": 0.0
        }});
        assert_ne!(action(&answers), Action::AwaitInput);
        answers["input"]["probabilities"]["required"] = 0.6.into();
        assert_eq!(action(&answers), Action::AwaitInput);
    }
}

//! Frozen GLiDE completed-turn decision rule (2026-11-12 fresh evaluation).
pub mod types;
use crate::assessment::Action;
use serde_json::Value;

pub const QUESTIONS: &str = include_str!("eot_glide_questions.json");
// FROZEN/rule.json, glide-eot-research-20261112-v4, fresh development fit.
pub const REQUIRED_INPUT: f64 = 0.80;
pub const FAILURE_FLOOR: f64 = 0.90;
pub const FINISHED_CATEGORICAL: f64 = 0.9162209700222781;
pub const WAITING_WORK_WINNER: f64 = 0.40;
pub const SAFE_INPUT_NONE: f64 = 0.85;
pub const CONTINUE_FLOOR: f64 = 0.90;
pub const BACKGROUND_FLOOR: f64 = 0.90;
/// Intentionally abstain until live outcomes justify automatic continuation.
pub const ENABLE_AUTOMATIC_CONTINUE: bool = false;

/// Apply the frozen rule to validated provider answers. Missing probabilities
/// abstain; callers must validate distributions at the provider boundary.
pub fn action(answers: &Value, authorization_complete: bool) -> Action {
    let p = |axis: &str, name: &str| answers[axis]["probabilities"][name].as_f64().unwrap_or(0.0);
    if p("failure", "quota") >= FAILURE_FLOOR {
        return Action::RecoverQuota;
    }
    if p("input", "required") >= REQUIRED_INPUT {
        return Action::AwaitInput;
    }
    if p("failure", "transient_provider") >= FAILURE_FLOOR {
        return Action::RetryProvider;
    }
    if p("failure", "other") >= FAILURE_FLOOR {
        return Action::AwaitInput;
    }
    let safe = p("failure", "none") >= FAILURE_FLOOR;
    let continuation = safe
        && p("input", "none").max(p("input", "redundant_request")) >= CONTINUE_FLOOR
        && p("turn_state", "continue").min(p("work", "authorized_unfinished")) >= CONTINUE_FLOOR
        && authorization_complete;
    // Continue has precedence over Finished and Wait in the frozen rule;
    // disabling it must abstain rather than expose a lower-priority action.
    if continuation {
        return if ENABLE_AUTOMATIC_CONTINUE {
            Action::Continue
        } else {
            Action::Uncertain
        };
    }
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
    fn frozen_fresh_test_probabilities_match_research_with_continue_abstention() {
        let cases: Value =
            serde_json::from_str(include_str!("eot_glide_fresh_sample.json")).unwrap();
        for case in cases.as_array().unwrap() {
            let expected: Action = serde_json::from_value(case["expected"].clone()).unwrap();
            assert_eq!(
                action(
                    &case["answers"],
                    case["authorization_complete"].as_bool().unwrap()
                ),
                expected,
                "{}",
                case["index"]
            );
            assert_eq!(
                background_needed(&case["answers"], case["has_background"].as_bool().unwrap()),
                serde_json::from_value::<Option<bool>>(case["background_needed"].clone()).unwrap()
            );
        }
    }
}

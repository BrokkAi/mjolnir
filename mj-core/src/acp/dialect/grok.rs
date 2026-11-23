//! Grok Build's ACP plan review extension.

use crate::config::HarnessKind;
use crate::elicitation::ElicitationResponse;

use super::super::plan_review_answer;

const EXIT_PLAN_MODE_METHOD: &str = "x.ai/exit_plan_mode";
const PLAN_REVIEW_ID_PREFIX: &str = "plan-review-grok-";

fn is_exit_plan_mode_method(method: &str) -> bool {
    method.strip_prefix('_').unwrap_or(method) == EXIT_PLAN_MODE_METHOD
}

pub fn handles_exit_plan_mode(harness: HarnessKind, method: &str) -> bool {
    harness == HarnessKind::Grok && is_exit_plan_mode_method(method)
}

pub fn plan_review_id(sequence: u64) -> String {
    format!("{PLAN_REVIEW_ID_PREFIX}{sequence}")
}

pub fn is_plan_review_id(id: &str) -> bool {
    id.starts_with(PLAN_REVIEW_ID_PREFIX)
}

pub fn plan_response(response: ElicitationResponse) -> serde_json::Value {
    let (action, feedback) = plan_review_answer(response);
    match action.as_str() {
        "implement" => serde_json::json!({ "outcome": "approved" }),
        "exit" => serde_json::json!({ "outcome": "abandoned" }),
        "revise" => feedback.map_or_else(
            || serde_json::json!({ "outcome": "cancelled" }),
            |feedback| serde_json::json!({ "outcome": "cancelled", "feedback": feedback }),
        ),
        _ => serde_json::json!({ "outcome": "cancelled" }),
    }
}

pub fn permits_unadvertised_plan_mode(harness: HarnessKind, mode_id: &str) -> bool {
    harness == HarnessKind::Grok && matches!(mode_id, "plan" | "default")
}

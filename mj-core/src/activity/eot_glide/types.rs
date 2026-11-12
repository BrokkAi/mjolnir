use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Distribution<T> {
    pub probabilities: T,
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct Probabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<Distribution<Attention>>,
    pub input: Distribution<Input>,
    pub failure: Distribution<Failure>,
    pub work: Distribution<Work>,
    pub background: Distribution<Background>,
    pub turn_state: Distribution<TurnState>,
    pub permission_veto: Distribution<PermissionVeto>,
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub none: f64,
    pub redundant_request: f64,
    pub required: f64,
    pub unclear: f64,
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub none: f64,
    pub transient_provider: f64,
    pub quota: f64,
    pub other: f64,
    pub unclear: f64,
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Work {
    pub finished: f64,
    pub authorized_unfinished: f64,
    pub waiting: f64,
    pub unclear: f64,
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Background {
    pub needed: f64,
    pub unneeded: f64,
    pub unclear: f64,
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TurnState {
    pub recover_quota: f64,
    pub await_input: f64,
    pub retry_provider: f64,
    pub wait: f64,
    #[serde(rename = "continue")]
    pub continue_work: f64,
    pub finished: f64,
    pub unclear: f64,
}
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionVeto {
    pub redundant_only: f64,
    pub not_redundant_only: f64,
    pub no_request: f64,
    pub unknown: f64,
}
use anyhow::{Context, Result, ensure};
use serde_json::Value;
/// Questions added after the frozen set. The hosted proxy and the workers
/// upgrade independently, so a proxy that predates one of these omits its
/// answer. That question then has no effect, instead of every verdict failing.
/// An answer that is present must still be complete and valid.
const OPTIONAL_AXES: &[&str] = &["attention"];
impl Probabilities {
    pub fn parse(response: &Value) -> Result<Self> {
        let answers = response.get("answers").context("missing GLiDE answers")?;
        let questions: Value = serde_json::from_str(super::QUESTIONS)?;
        for (axis, question) in questions.as_object().unwrap() {
            if OPTIONAL_AXES.contains(&axis.as_str()) && answers.get(axis).is_none() {
                continue;
            }
            let scores = answers[axis]["probabilities"]
                .as_object()
                .context("missing GLiDE distribution")?;
            let criteria = question["criteria"].as_object().unwrap();
            ensure!(
                scores.len() == criteria.len()
                    && criteria.keys().all(|k| scores
                        .get(k)
                        .and_then(Value::as_f64)
                        .is_some_and(|p| p.is_finite() && (0.0..=1.0).contains(&p))),
                "invalid GLiDE distribution for {axis}"
            );
            ensure!(
                (scores.values().map(|p| p.as_f64().unwrap()).sum::<f64>() - 1.0).abs() <= 0.03,
                "unnormalized GLiDE distribution for {axis}"
            );
        }
        Ok(serde_json::from_value(answers.clone())?)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Attention {
    pub needs_input: f64,
    pub important: f64,
    pub redundant_request: f64,
    pub none: f64,
    pub unclear: f64,
}

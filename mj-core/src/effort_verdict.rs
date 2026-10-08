//! Shared request contract and effort mapping for per-child Jev judgments.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const QUESTIONS: &str = include_str!("effort_verdict/questions.json");

const MAX_TASK_NAME_BYTES: usize = 256;
const MAX_MODEL_BYTES: usize = 256;
const INSTRUCTIONS_HEAD_BYTES: usize = 24 * 1024;
const INSTRUCTIONS_TAIL_BYTES: usize = 8 * 1024;
const INSTRUCTIONS_MARKER: &str =
    "\n\n[Middle of the assignment omitted to fit the Jev request limit.]\n\n";
const RUNGS: [&str; 4] = ["medium", "high", "xhigh", "max"];
const KNOWN_EFFORTS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// The bounded hosted body and the state sent to TypeSafe in the direct form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffortEvidence {
    pub task_name: String,
    pub instructions: String,
    pub model: String,
    pub instructions_truncated: bool,
}

#[derive(Serialize)]
struct UpstreamBody<'a> {
    model: &'static str,
    state: &'a EffortEvidence,
    questions: OrderedQuestions,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OrderedQuestions {
    effort: OrderedQuestion,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OrderedQuestion {
    #[serde(rename = "type")]
    kind: String,
    instructions: String,
    criteria: OrderedCriteria,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OrderedCriteria {
    medium: OrderedCriterion,
    high: OrderedCriterion,
    xhigh: OrderedCriterion,
    max: OrderedCriterion,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct OrderedCriterion {
    what: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EffortVerdict {
    pub choice: String,
    pub confidence: f64,
    pub probabilities: [f64; 4],
}

pub fn questions() -> Value {
    serde_json::to_value(ordered_questions()).expect("effort questions serialize")
}

fn ordered_questions() -> OrderedQuestions {
    serde_json::from_str(QUESTIONS).expect("checked-in effort questions are valid JSON")
}

impl EffortEvidence {
    /// Bound both wire representations so unusual escaped text cannot exceed
    /// the transport cap after JSON serialization.
    pub fn bounded(task_name: &str, instructions: &str, model: &str) -> Result<Self> {
        ensure!(
            !task_name.trim().is_empty() && task_name.len() <= MAX_TASK_NAME_BYTES,
            "invalid effort verdict task name"
        );
        ensure!(
            !model.trim().is_empty() && model.len() <= MAX_MODEL_BYTES,
            "invalid effort verdict model"
        );
        ensure!(
            !instructions.trim().is_empty(),
            "empty effort verdict instructions"
        );

        let mut head_bytes = instructions.len().min(INSTRUCTIONS_HEAD_BYTES);
        let mut tail_bytes = if instructions.len() > INSTRUCTIONS_HEAD_BYTES {
            (instructions.len() - INSTRUCTIONS_HEAD_BYTES).min(INSTRUCTIONS_TAIL_BYTES)
        } else {
            0
        };
        loop {
            let (bounded_instructions, instructions_truncated) =
                retain_instructions(instructions, head_bytes, tail_bytes);
            let evidence = Self {
                task_name: task_name.to_owned(),
                instructions: bounded_instructions,
                model: model.to_owned(),
                instructions_truncated,
            };
            let hosted_len = serde_json::to_vec(&evidence)?.len();
            let upstream_len = serde_json::to_vec(&UpstreamBody {
                model: "jev-latest",
                state: &evidence,
                questions: ordered_questions(),
            })?
            .len();
            if hosted_len <= MAX_BODY_BYTES && upstream_len <= MAX_BODY_BYTES {
                return Ok(evidence);
            }

            ensure!(
                head_bytes > 0 || tail_bytes > 0,
                "effort verdict metadata exceeds the request limit"
            );
            let next_head = head_bytes.saturating_mul(3) / 4;
            let next_tail = tail_bytes.saturating_mul(3) / 4;
            head_bytes = if next_head == head_bytes && head_bytes > 0 {
                head_bytes - 1
            } else {
                next_head
            };
            tail_bytes = if next_tail == tail_bytes && tail_bytes > 0 {
                tail_bytes - 1
            } else {
                next_tail
            };
        }
    }

    pub fn hosted_body(&self) -> Result<Vec<u8>> {
        let body = serde_json::to_vec(self)?;
        ensure!(
            body.len() <= MAX_BODY_BYTES,
            "oversized effort verdict request"
        );
        Ok(body)
    }

    pub fn upstream_body(&self) -> Result<Vec<u8>> {
        let body = serde_json::to_vec(&UpstreamBody {
            model: "jev-latest",
            state: self,
            questions: ordered_questions(),
        })?;
        ensure!(
            body.len() <= MAX_BODY_BYTES,
            "oversized effort verdict request"
        );
        Ok(body)
    }
}

impl EffortVerdict {
    /// Parse the single Jev choice and reject incomplete or inconsistent
    /// probability data before it can influence a child launch.
    pub fn parse(response: &Value) -> Result<Self> {
        let answers = response
            .get("answers")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow::anyhow!("missing effort verdict answers"))?;
        ensure!(
            answers.len() == 1 && answers.contains_key("effort"),
            "incomplete effort verdict answers"
        );
        let answer = answers["effort"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("invalid effort verdict answer"))?;
        if let Some(kind) = answer.get("type") {
            ensure!(kind == "choice", "invalid effort verdict answer type");
        }
        let choice = answer
            .get("choice")
            .and_then(Value::as_str)
            .filter(|choice| RUNGS.contains(choice))
            .ok_or_else(|| anyhow::anyhow!("invalid effort verdict choice"))?
            .to_owned();
        let confidence = answer
            .get("confidence")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
            .ok_or_else(|| anyhow::anyhow!("invalid effort verdict confidence"))?;
        let probabilities = answer
            .get("probabilities")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow::anyhow!("missing effort verdict probabilities"))?;
        ensure!(
            probabilities.len() == RUNGS.len(),
            "incomplete effort verdict probabilities"
        );
        let mut values = [0.0; 4];
        for (index, rung) in RUNGS.iter().enumerate() {
            values[index] = probabilities
                .get(*rung)
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
                .ok_or_else(|| anyhow::anyhow!("invalid probability for effort {rung}"))?;
        }
        let selected = values[RUNGS.iter().position(|rung| *rung == choice).unwrap()];
        ensure!(
            (values.iter().sum::<f64>() - 1.0).abs() <= 0.020_000_000_001,
            "effort verdict probabilities do not sum to one"
        );
        ensure!(
            values.iter().all(|value| *value <= selected + 1e-12),
            "effort verdict choice is not the most likely rung"
        );
        Ok(Self {
            choice,
            confidence,
            probabilities: values,
        })
    }
}

/// Map one canonical rung to the closest position in the model's advertised
/// effort ladder. Unknown provider names retain their advertised order.
pub fn map_rung_to_offered(
    offered: &[crate::acp::SessionConfigChoice],
    rung: &str,
) -> Option<String> {
    let rung_index = RUNGS.iter().position(|candidate| *candidate == rung)?;
    let mut choices = offered
        .iter()
        .map(|choice| choice.value.as_str())
        .filter(|value| !value.eq_ignore_ascii_case("default"))
        .collect::<Vec<_>>();
    if choices.is_empty() {
        return None;
    }
    if choices.iter().all(|value| KNOWN_EFFORTS.contains(value)) {
        // Vendor names on the standard scale: take the offered effort whose
        // rank is nearest the rung's, and the cheaper one on a tie, so a
        // model offering only `high` and `max` starts at `high` for `xhigh`.
        let rank = |value: &str| {
            KNOWN_EFFORTS
                .iter()
                .position(|known| *known == value)
                .expect("all effort names were checked as known") as i64
        };
        let target = rank(rung);
        return choices
            .iter()
            .min_by_key(|value| {
                let distance = rank(value) - target;
                (distance.abs(), distance)
            })
            .map(|value| (*value).to_owned());
    }
    // Unknown names: trust the advertised order as ascending, keep the top
    // four, and map the ladder onto them by position.
    if choices.len() > RUNGS.len() {
        choices.drain(..choices.len() - RUNGS.len());
    }
    let index = ((rung_index * (choices.len() - 1)) as f64 / 3.0).round() as usize;
    Some(choices[index].to_owned())
}

fn retain_instructions(text: &str, head_bytes: usize, tail_bytes: usize) -> (String, bool) {
    if head_bytes.saturating_add(tail_bytes) >= text.len() {
        return (text.to_owned(), false);
    }
    let head = utf8_prefix(text, head_bytes);
    let tail = utf8_suffix(text, tail_bytes);
    (format!("{head}{INSTRUCTIONS_MARKER}{tail}"), true)
}

fn utf8_prefix(text: &str, limit: usize) -> &str {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn utf8_suffix(text: &str, limit: usize) -> &str {
    let mut start = text.len().saturating_sub(limit);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn efforts(values: &[&str]) -> Vec<crate::acp::SessionConfigChoice> {
        values
            .iter()
            .map(|value| crate::acp::SessionConfigChoice {
                value: (*value).to_owned(),
                name: (*value).to_owned(),
                description: None,
            })
            .collect()
    }

    #[test]
    fn questions_json_ships_in_published_package() {
        // `QUESTIONS` is embedded with `include_str!`, and `cargo package`
        // verifies the crate builds from the packaged subset, so the file
        // must be in the `include` list in mj-core/Cargo.toml.
        const MANIFEST: &str = include_str!("../Cargo.toml");
        assert!(
            MANIFEST.contains("src/effort_verdict/questions.json"),
            "mj-core/Cargo.toml `include` must list src/effort_verdict/questions.json"
        );
    }

    #[test]
    fn effort_rungs_map_after_default_removal_ordering_and_ladder_capping() {
        let offered = efforts(&[
            "max", "default", "medium", "low", "xhigh", "high", "minimal",
        ]);
        assert_eq!(
            RUNGS.map(|rung| map_rung_to_offered(&offered, rung).unwrap()),
            ["medium", "high", "xhigh", "max"]
        );

        let two = efforts(&["high", "low"]);
        assert_eq!(
            RUNGS.map(|rung| map_rung_to_offered(&two, rung).unwrap()),
            ["low", "high", "high", "high"]
        );
        let three = efforts(&["high", "low", "medium"]);
        assert_eq!(
            RUNGS.map(|rung| map_rung_to_offered(&three, rung).unwrap()),
            ["medium", "high", "high", "high"]
        );
        let deepseek = efforts(&["high", "max"]);
        assert_eq!(
            RUNGS.map(|rung| map_rung_to_offered(&deepseek, rung).unwrap()),
            ["high", "high", "high", "max"]
        );

        let unknown = efforts(&["provider-top", "default", "provider-bottom"]);
        assert_eq!(
            map_rung_to_offered(&unknown, "max").as_deref(),
            Some("provider-bottom")
        );
        assert_eq!(map_rung_to_offered(&efforts(&["DEFAULT"]), "high"), None);
    }
}

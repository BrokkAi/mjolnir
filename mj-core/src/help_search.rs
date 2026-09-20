//! Bounded, index-free help search shared by the terminal and hosted proxy.

use std::collections::BTreeSet;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const MAX_QUERY_BYTES: usize = 1024;
pub const MAX_ENTRIES: usize = 128;
pub const QUESTION: &str = include_str!("help_search/question.json");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelpSearchEntry {
    pub id: usize,
    pub category: String,
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelpSearchRequest {
    pub query: String,
    pub entries: Vec<HelpSearchEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelpSearchScore {
    pub id: usize,
    pub probability: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelpSearchResponse {
    pub scores: Vec<HelpSearchScore>,
}

impl HelpSearchRequest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.query.trim().is_empty() && self.query.len() <= MAX_QUERY_BYTES,
            "invalid help query length"
        );
        ensure!(
            !self.entries.is_empty() && self.entries.len() <= MAX_ENTRIES,
            "invalid help catalog length"
        );
        let mut ids = BTreeSet::new();
        for entry in &self.entries {
            ensure!(
                entry.id < MAX_ENTRIES && ids.insert(entry.id),
                "invalid help entry id"
            );
            ensure!(
                !entry.category.is_empty()
                    && entry.category.len() <= 128
                    && !entry.label.is_empty()
                    && entry.label.len() <= 256
                    && entry.description.len() <= 1024,
                "invalid help entry text"
            );
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_BODY_BYTES,
            "help request exceeds byte limit"
        );
        Ok(())
    }

    /// Each question names its entry explicitly; question IDs are not model input.
    pub fn upstream_body(&self) -> Value {
        let template: Value = serde_json::from_str(QUESTION).expect("checked-in help question");
        let questions: serde_json::Map<String, Value> = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let mut question = template.clone();
                question["instructions"] = Value::String(
                    template["instructions"]
                        .as_str()
                        .expect("help instructions")
                        .replace("INDEX", &index.to_string()),
                );
                (format!("entry_{}", entry.id), question)
            })
            .collect();
        json!({"model": "jev-latest", "state": self, "questions": questions})
    }

    pub fn parse_upstream(&self, body: &Value) -> Result<HelpSearchResponse> {
        let answers = body["answers"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("missing help answers"))?;
        ensure!(
            answers.len() == self.entries.len(),
            "incomplete help answers"
        );
        let scores = self
            .entries
            .iter()
            .map(|entry| {
                let answer = &body["answers"][format!("entry_{}", entry.id)];
                ensure!(answer["type"] == "noul", "invalid help answer type");
                let probability = answer["noul"]
                    .as_f64()
                    .ok_or_else(|| anyhow::anyhow!("missing help probability"))?;
                Ok(HelpSearchScore {
                    id: entry.id,
                    probability,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let response = HelpSearchResponse { scores };
        response.validate(self)?;
        Ok(response)
    }
}

impl HelpSearchResponse {
    pub fn validate(&self, request: &HelpSearchRequest) -> Result<()> {
        let expected: BTreeSet<_> = request.entries.iter().map(|entry| entry.id).collect();
        let mut seen = BTreeSet::new();
        ensure!(
            self.scores.len() == expected.len(),
            "incomplete help scores"
        );
        for score in &self.scores {
            ensure!(
                expected.contains(&score.id) && seen.insert(score.id),
                "invalid help score id"
            );
            ensure!(
                score.probability.is_finite() && (0.0..=1.0).contains(&score.probability),
                "invalid help probability"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> HelpSearchRequest {
        HelpSearchRequest {
            query: "leave agents running".into(),
            entries: vec![HelpSearchEntry {
                id: 3,
                category: "Essentials".into(),
                label: "Detach".into(),
                description: "Leave this terminal; sessions keep running.".into(),
            }],
        }
    }

    #[test]
    fn questions_reference_entries_and_answers_require_exact_valid_coverage() {
        let request = request();
        request.validate().unwrap();
        let body = request.upstream_body();
        assert!(
            body["questions"]["entry_3"]["instructions"]
                .as_str()
                .unwrap()
                .contains("entries[0]")
        );
        assert_eq!(body["state"]["query"], request.query);
        for probability in [0.0, 0.7, 1.0] {
            assert_eq!(
                request
                    .parse_upstream(
                        &json!({"answers":{"entry_3":{"type":"noul","noul":probability}}})
                    )
                    .unwrap()
                    .scores[0]
                    .probability,
                probability
            );
        }
        for body in [
            json!({}),
            json!({"answers":{}}),
            json!({"answers":{"entry_3":{"type":"noul","noul":1.1}}}),
            json!({"answers":{"entry_4":{"type":"noul","noul":0.9}}}),
        ] {
            assert!(request.parse_upstream(&body).is_err());
        }
        let score = HelpSearchScore {
            id: 3,
            probability: 0.9,
        };
        assert!(
            HelpSearchResponse {
                scores: vec![score.clone(), score]
            }
            .validate(&request)
            .is_err()
        );
    }

    #[test]
    fn query_limits_count_utf8_bytes_and_catalog_ids_are_unique() {
        let mut request = request();
        request.query = "😀".repeat(256);
        request.validate().unwrap();
        request.query.push('x');
        assert!(request.validate().is_err());
        request.query = "hello".into();
        request.entries.push(request.entries[0].clone());
        assert!(request.validate().is_err());
    }
}

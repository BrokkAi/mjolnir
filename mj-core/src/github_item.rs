//! Bounded evidence for classifying GitHub issues and pull requests per session.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const QUESTIONS: &str = include_str!("github_item/questions.json");
// Keep the item's serialized prefix fixed across sessions, with room left for
// recent turns after the shared model and question wrapper.
const MAX_ITEM_PREFIX_BYTES: usize = MAX_BODY_BYTES - 16 * 1024;
const BODY_TRUNCATION_MARKER: &str =
    "\n\n[GitHub item body truncated to fit the 64 KiB request limit.]";
const CONTEXT_TRUNCATION_MARKER: &str =
    "[Older session turns omitted to fit the 64 KiB request limit.]\n\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubItemKind {
    Issue,
    PullRequest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubItem {
    pub repo: String,
    pub kind: GithubItemKind,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub author: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionContext {
    pub recent_turns: String,
}

/// The GitHub item deliberately serializes before its session-specific context,
/// allowing requests for different sessions to share the same item prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubItemEvidence {
    pub item: GithubItem,
    pub session: SessionContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GithubItemVerdict {
    pub interested: bool,
    pub created: bool,
}

#[derive(Serialize)]
struct UpstreamBody<'a> {
    model: &'static str,
    state: &'a GithubItemEvidence,
    questions: Value,
}

pub fn questions() -> Value {
    serde_json::from_str(QUESTIONS).expect("checked-in GitHub item questions are valid JSON")
}

impl GithubItemEvidence {
    /// Return evidence whose complete direct TypeSafe request, including the
    /// model and questions, fits the shared request limit.
    pub fn bounded_for_request(&self) -> Result<Self> {
        self.validate_fixed_fields()?;
        let mut bounded = self.clone();
        bounded.session.recent_turns.clear();
        if upstream_len(&bounded)? > MAX_ITEM_PREFIX_BYTES {
            let mut best = self.with_body_retained_bytes(0);
            best.session.recent_turns.clear();
            ensure!(
                upstream_len(&best)? <= MAX_ITEM_PREFIX_BYTES,
                "GitHub item metadata exceeds the Jev request limit"
            );
            let mut low = 0;
            let mut high = self.item.body.len();
            for _ in 0..32 {
                let retained_bytes = low + (high - low).div_ceil(2);
                let candidate = self.with_body_retained_bytes(retained_bytes);
                let mut prefix = candidate;
                prefix.session.recent_turns.clear();
                if upstream_len(&prefix)? <= MAX_ITEM_PREFIX_BYTES {
                    low = retained_bytes;
                    best = prefix;
                } else {
                    high = retained_bytes - 1;
                }
            }
            bounded = best;
        }

        bounded.session.recent_turns = self.session.recent_turns.clone();
        if upstream_len(&bounded)? <= MAX_BODY_BYTES {
            return Ok(bounded);
        }

        let mut best = bounded.with_context_retained_bytes(0);
        ensure!(
            upstream_len(&best)? <= MAX_BODY_BYTES,
            "GitHub item request metadata exceeds the Jev request limit"
        );
        let mut low = 0;
        let mut high = self.session.recent_turns.len();
        for _ in 0..32 {
            let retained_bytes = low + (high - low).div_ceil(2);
            let candidate = bounded.with_context_retained_bytes(retained_bytes);
            if upstream_len(&candidate)? <= MAX_BODY_BYTES {
                low = retained_bytes;
                best = candidate;
            } else {
                high = retained_bytes - 1;
            }
        }
        ensure!(
            upstream_len(&best)? <= MAX_BODY_BYTES,
            "GitHub item request exceeds byte limit"
        );
        Ok(best)
    }

    pub fn validate(&self) -> Result<()> {
        self.validate_fixed_fields()?;
        ensure!(
            upstream_len(self)? <= MAX_BODY_BYTES,
            "GitHub item request exceeds byte limit"
        );
        Ok(())
    }

    /// Build the direct-provider envelope without converting the ordered
    /// evidence structs into a map-backed `Value`.
    pub fn upstream_body(&self) -> impl Serialize + '_ {
        UpstreamBody {
            model: "jev-latest",
            state: self,
            questions: questions(),
        }
    }

    fn validate_fixed_fields(&self) -> Result<()> {
        ensure!(
            !self.item.repo.trim().is_empty() && self.item.repo.len() <= 256,
            "invalid GitHub repository"
        );
        ensure!(
            self.item.number > 0 && self.item.number <= 9_007_199_254_740_991,
            "invalid GitHub item number"
        );
        ensure!(
            !self.item.title.trim().is_empty() && self.item.title.len() <= 1024,
            "invalid GitHub item title"
        );
        ensure!(
            self.item.author.len() <= 256,
            "GitHub item author is too long"
        );
        ensure!(
            !self.item.url.trim().is_empty() && self.item.url.len() <= 2048,
            "invalid GitHub item URL"
        );
        Ok(())
    }

    fn with_body_retained_bytes(&self, retained_bytes: usize) -> Self {
        let mut bounded = self.clone();
        bounded.item.body = truncate_with_marker(
            &self.item.body,
            retained_bytes,
            BODY_TRUNCATION_MARKER,
            false,
        );
        bounded
    }

    fn with_context_retained_bytes(&self, retained_bytes: usize) -> Self {
        let mut bounded = self.clone();
        bounded.session.recent_turns = truncate_with_marker(
            &self.session.recent_turns,
            retained_bytes,
            CONTEXT_TRUNCATION_MARKER,
            true,
        );
        bounded
    }
}

impl GithubItemVerdict {
    pub fn parse(response: &Value) -> Result<Self> {
        fn affirmative_probability(response: &Value, key: &str) -> Result<bool> {
            let answer = &response["answers"][key];
            ensure!(answer["type"] == "noul", "invalid GitHub item answer type");
            let probability = answer["noul"]
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("missing GitHub item probability"))?;
            ensure!(
                probability.is_finite() && (0.0..=1.0).contains(&probability),
                "invalid GitHub item probability"
            );
            Ok(probability >= 0.5)
        }

        let answers = response["answers"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("missing GitHub item answers"))?;
        ensure!(
            answers.len() == 2
                && answers.contains_key("interested")
                && answers.contains_key("created"),
            "incomplete GitHub item answers"
        );
        Ok(Self {
            interested: affirmative_probability(response, "interested")?,
            created: affirmative_probability(response, "created")?,
        })
    }
}

fn upstream_len(evidence: &GithubItemEvidence) -> Result<usize> {
    Ok(serde_json::to_vec(&evidence.upstream_body())?.len())
}

fn truncate_with_marker(
    text: &str,
    retained_bytes: usize,
    marker: &str,
    from_oldest: bool,
) -> String {
    if retained_bytes >= text.len() {
        return text.to_owned();
    }
    if text.is_empty() {
        return String::new();
    }
    let retained = if from_oldest {
        utf8_suffix(text, retained_bytes)
    } else {
        utf8_prefix(text, retained_bytes)
    };
    if from_oldest {
        format!("{marker}{retained}")
    } else {
        format!("{retained}{marker}")
    }
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
    use serde_json::json;

    fn evidence(body: String, recent_turns: String) -> GithubItemEvidence {
        GithubItemEvidence {
            item: GithubItem {
                repo: "brokkai/mjolnir".into(),
                kind: GithubItemKind::PullRequest,
                number: 42,
                title: "Add mailbox support".into(),
                body,
                author: "agent".into(),
                url: "https://github.com/brokkai/mjolnir/pull/42".into(),
            },
            session: SessionContext { recent_turns },
        }
    }

    #[test]
    fn bounded_request_marks_truncation_and_keeps_newest_context_without_splitting_utf8() {
        let original = evidence(
            "🛠".repeat(30_000),
            "old turn\n".to_owned() + &"new turn 🧭\n".repeat(5_000),
        );
        let bounded = original.bounded_for_request().unwrap();
        assert!(upstream_len(&bounded).unwrap() <= MAX_BODY_BYTES);
        assert!(bounded.item.body.ends_with(BODY_TRUNCATION_MARKER));
        assert!(
            bounded
                .session
                .recent_turns
                .starts_with(CONTEXT_TRUNCATION_MARKER)
        );
        assert!(!bounded.session.recent_turns.contains("old turn"));
        assert!(bounded.session.recent_turns.ends_with("new turn 🧭\n"));

        let short_context = evidence(original.item.body.clone(), "three recent turns".into())
            .bounded_for_request()
            .unwrap();
        assert_eq!(bounded.item, short_context.item);
    }

    #[test]
    fn evidence_serialization_keeps_item_before_session_and_parses_only_typed_answers() {
        let evidence = evidence(
            "Add mailbox support".into(),
            "<turn number=\"1\">work</turn>".into(),
        );
        let serialized = serde_json::to_string(&evidence).unwrap();
        assert!(serialized.find("\"item\"").unwrap() < serialized.find("\"session\"").unwrap());
        let direct = serde_json::to_string(&evidence.upstream_body()).unwrap();
        let state = &direct[direct.find("\"state\":").unwrap()..];
        let ordered_fields = [
            "\"item\":",
            "\"repo\":",
            "\"kind\":",
            "\"number\":",
            "\"title\":",
            "\"body\":",
            "\"author\":",
            "\"url\":",
            "\"session\":",
            "\"recent_turns\":",
        ];
        let positions: Vec<_> = ordered_fields
            .iter()
            .map(|field| state.find(field).unwrap())
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        let verdict = GithubItemVerdict::parse(&json!({"answers": {
            "interested": {"type":"noul","noul":0.91},
            "created": {"type":"noul","noul":0.04}
        }}))
        .unwrap();
        assert_eq!(
            verdict,
            GithubItemVerdict {
                interested: true,
                created: false
            }
        );
        for answer in [
            json!({"answers":{"interested":{"type":"choice","choice":"yes"},"created":{"type":"noul","noul":0.1}}}),
            json!({"answers":{"interested":{"type":"noul","noul":1.1},"created":{"type":"noul","noul":0.1}}}),
            json!({"answers":{"interested":{"type":"noul","noul":0.1}}}),
        ] {
            assert!(GithubItemVerdict::parse(&answer).is_err());
        }
    }
}

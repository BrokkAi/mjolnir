//! Bounded evidence for classifying GitHub issues and pull requests per session.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const QUESTIONS: &str = include_str!("github_item/questions_v2.json");
// Relevance evidence budgets; conversation limits are selected by replay.
// Mirrored by the v2 proxy validator.
pub const ITEM_BODY_BYTES: usize = 4096;
pub const SESSION_TURNS: usize = 3;
pub const CONVERSATION_BYTES: usize = 8 * 1024;
pub const MESSAGE_BYTES: usize = 2 * 1024;
pub const MAX_FILES: usize = 32;
pub const FILE_BYTES: usize = 256;
pub const MAX_REFERENCES: usize = 32;
pub const REFERENCE_BYTES: usize = 128;
// Design budgets sizing item-conditioned evidence, motivated by the 83-pair replay.
pub const MAX_MENTIONS: usize = 16;
pub const MENTION_BYTES: usize = 256;
const BODY_TRUNCATION_MARKER: &str = "\n[GitHub item body truncated.]";

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

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistorySnippet {
    pub time: Option<String>,
    pub text: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionContext {
    pub conversation: String,
    pub files: Vec<String>,
    pub references: Vec<String>,
    #[serde(default)]
    pub mentions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<HistorySnippet>>,
    /// Per-session reference index; never included in a provider request.
    #[serde(skip)]
    pub mention_candidates: Vec<(crate::github_reference::GithubReference, String)>,
}

impl SessionContext {
    /// Select the lines that mention `item` or an issue its title or body
    /// references. A bare `#N` in the item means the item's repository. In
    /// the session it does only when `bare_is_item_repo`: a session whose
    /// project spans several GitHub repositories cannot say which one `#42`
    /// names, and must not have the line presented as a mention of every
    /// watched repository's #42.
    pub fn for_item(mut self, item: &GithubItem, bare_is_item_repo: bool) -> Self {
        use crate::github_reference::{GithubReference, github_references};
        let mut targets = github_references(&format!("{}\n{}", item.title, item.body));
        targets.extend(github_references(&format!("{}#{}", item.repo, item.number)));
        let identity = |r: &GithubReference, bare_is_item_repo: bool| {
            let repo = match (&r.owner, &r.repo) {
                (Some(owner), Some(repo)) => format!("{owner}/{repo}"),
                _ if bare_is_item_repo => item.repo.clone(),
                _ => return None,
            };
            Some((repo.to_ascii_lowercase(), r.number))
        };
        let targets: Vec<_> = targets.iter().filter_map(|t| identity(t, true)).collect();
        self.mentions.clear();
        for (reference, line) in self.mention_candidates.iter().rev() {
            if identity(reference, bare_is_item_repo).is_some_and(|r| targets.contains(&r))
                && !self.mentions.contains(line)
            {
                self.mentions.push(line.clone());
                if self.mentions.len() == MAX_MENTIONS {
                    break;
                }
            }
        }
        self.mention_candidates.clear();
        self
    }
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
        if bounded.item.body.len() > ITEM_BODY_BYTES {
            bounded.item.body = format!(
                "{}{}",
                utf8_prefix(&bounded.item.body, ITEM_BODY_BYTES),
                BODY_TRUNCATION_MARKER
            );
        }
        bounded.validate()?;
        Ok(bounded)
    }

    pub fn validate(&self) -> Result<()> {
        self.validate_fixed_fields()?;
        ensure!(
            self.session.conversation.len() <= CONVERSATION_BYTES,
            "GitHub session conversation is too long"
        );
        ensure!(
            self.session.files.len() <= MAX_FILES
                && self
                    .session
                    .files
                    .iter()
                    .all(|s| !s.trim().is_empty() && s.len() <= FILE_BYTES),
            "invalid GitHub session files"
        );
        ensure!(
            self.session.references.len() <= MAX_REFERENCES
                && self
                    .session
                    .references
                    .iter()
                    .all(|s| !s.trim().is_empty() && s.len() <= REFERENCE_BYTES),
            "invalid GitHub session references"
        );
        ensure!(
            self.session.mentions.len() <= MAX_MENTIONS
                && self
                    .session
                    .mentions
                    .iter()
                    .all(|s| !s.trim().is_empty() && s.len() <= MENTION_BYTES),
            "invalid GitHub session mentions"
        );
        ensure!(
            self.item.body.len() <= ITEM_BODY_BYTES + BODY_TRUNCATION_MARKER.len(),
            "GitHub item body is too long"
        );
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
            // The 83-pair production replay found relevant items just below 0.5.
            Ok(probability >= 0.4)
        }

        let answers = response["answers"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("missing GitHub item answers"))?;
        ensure!(
            answers.len() == 1 && answers.contains_key("interested"),
            "incomplete GitHub item answers"
        );
        Ok(Self {
            interested: affirmative_probability(response, "interested")?,
        })
    }
}

fn upstream_len(evidence: &GithubItemEvidence) -> Result<usize> {
    Ok(serde_json::to_vec(&evidence.upstream_body())?.len())
}

fn utf8_prefix(text: &str, limit: usize) -> &str {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn evidence() -> GithubItemEvidence {
        GithubItemEvidence {
            item: GithubItem {
                repo: "brokkai/mjolnir".into(),
                kind: GithubItemKind::PullRequest,
                number: 42,
                title: "Add mailbox support".into(),
                body: "Add mailbox support".into(),
                author: "agent".into(),
                url: "https://github.com/brokkai/mjolnir/pull/42".into(),
            },
            session: SessionContext {
                conversation: "User: Implement mailbox support\nAgent: Checking the worker\n"
                    .into(),
                files: vec!["mj-worker/src/lib.rs".into()],
                references: vec!["#42".into()],
                ..Default::default()
            },
        }
    }

    #[test]
    fn item_mentions_resolve_bare_references_without_crossing_repositories() {
        let item = GithubItem {
            repo: "owner/repo".into(),
            kind: GithubItemKind::Issue,
            number: 42,
            title: "Fix #7642".into(),
            body: String::new(),
            author: String::new(),
            url: "https://github.com/owner/repo/issues/42".into(),
        };
        let mut session = SessionContext::default();
        for line in [
            "pending #7642",
            "other/repo#42",
            "owner/repo#42",
            "pending #7642",
        ] {
            for reference in crate::github_reference::github_references(line) {
                session.mention_candidates.push((reference, line.into()));
            }
        }
        // A multi-repository session's bare #7642 names no repository.
        let ambiguous = session.clone().for_item(&item, false);
        assert_eq!(ambiguous.mentions, ["owner/repo#42"]);
        let selected = session.for_item(&item, true);
        assert_eq!(selected.mentions, ["pending #7642", "owner/repo#42"]);
        assert!(
            !serde_json::to_string(&selected)
                .unwrap()
                .contains("mention_candidates")
        );
    }

    #[test]
    fn bounded_request_marks_truncation_without_splitting_utf8() {
        let typical = evidence();
        println!(
            "typical v2 request: {} bytes",
            upstream_len(&typical).unwrap()
        );
        let mut maximal = typical.clone();
        maximal.item.repo = "r".repeat(256);
        maximal.item.title = "t".repeat(1024);
        maximal.item.author = "a".repeat(256);
        maximal.item.url = "u".repeat(2048);
        maximal.item.number = 9_007_199_254_740_991;
        maximal.item.body = "x".repeat(ITEM_BODY_BYTES - 1) + "🦀";
        maximal.session = SessionContext {
            conversation: "x".repeat(CONVERSATION_BYTES),
            files: vec!["x".repeat(FILE_BYTES); MAX_FILES],
            references: vec!["x".repeat(REFERENCE_BYTES); MAX_REFERENCES],
            ..Default::default()
        };
        let bounded = maximal.bounded_for_request().unwrap();
        assert_eq!(
            bounded.item.body,
            "x".repeat(ITEM_BODY_BYTES - 1) + BODY_TRUNCATION_MARKER
        );
        println!(
            "maximal v2 request: {} bytes",
            upstream_len(&bounded).unwrap()
        );
        assert!(upstream_len(&bounded).unwrap() <= MAX_BODY_BYTES);
        let mut escaped = bounded.clone();
        escaped.item.repo = "\"".repeat(256);
        escaped.item.title = "\"".repeat(1024);
        escaped.item.author = "\"".repeat(256);
        escaped.item.url = "\"".repeat(2048);
        escaped.item.body = format!("{}{}", "\"".repeat(ITEM_BODY_BYTES), BODY_TRUNCATION_MARKER);
        escaped.session.conversation = "\"".repeat(CONVERSATION_BYTES);
        escaped.session.files = vec!["\"".repeat(FILE_BYTES); MAX_FILES];
        escaped.session.references = vec!["\"".repeat(REFERENCE_BYTES); MAX_REFERENCES];
        escaped.validate().unwrap();
        println!(
            "maximal escaped v2 request: {} bytes",
            upstream_len(&escaped).unwrap()
        );
        // Individual UTF-8 budgets do not bound JSON's six-byte control escapes.
        // The complete envelope must still reject oversized combinations.
        escaped.item.body = "\0".repeat(ITEM_BODY_BYTES);
        assert!(upstream_len(&escaped).unwrap() > MAX_BODY_BYTES);
        assert!(escaped.validate().is_err());
        let serialized = serde_json::to_string(&bounded.upstream_body()).unwrap();
        assert!(serialized.find("\"item\"").unwrap() < serialized.find("\"session\"").unwrap());
    }

    #[test]
    fn evidence_serialization_parses_only_typed_v2_answers() {
        assert!(
            GithubItemVerdict::parse(&json!({"answers":{"interested":{"type":"noul","noul":0.5}}}))
                .unwrap()
                .interested
        );
        for response in [
            json!({"answers":{"interested":{"type":"choice","choice":"yes"}}}),
            json!({"answers":{"interested":{"type":"noul","noul":1.1}}}),
            json!({"answers":{"interested":{"type":"noul","noul":0.1},"created":{"type":"noul","noul":0.2}}}),
        ] {
            assert!(GithubItemVerdict::parse(&response).is_err());
        }
    }
}

#[path = "github_item/history_unicode.rs"]
pub mod history_unicode;

/// Frozen tokenizer for aliases600 evidence budgets, without truncation or padding.
#[cfg(feature = "github-history")]
pub fn history_tokenizer() -> Result<tokenizers::Tokenizer> {
    let mut t =
        tokenizers::Tokenizer::from_bytes(include_bytes!("github_item/ettin-tokenizer.json"))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    t.with_padding(None);
    t.with_truncation(None)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(t)
}

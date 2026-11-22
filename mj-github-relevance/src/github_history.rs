//! Budget the retrieved GitHub history for the aliases600 classifier.

use anyhow::Result;
use mj_transcript::github_history::{OrderedValue as Value, Span, redact, retrieve};

/// Frozen tokenizer for aliases600 evidence budgets, without truncation or padding.
/// Built once: parsing the embedded 3.5 MB definition on every evidence build
/// was a measurable share of the daemon's page faults.
fn history_tokenizer() -> Result<&'static tokenizers::Tokenizer> {
    static TOKENIZER: std::sync::OnceLock<std::result::Result<tokenizers::Tokenizer, String>> =
        std::sync::OnceLock::new();
    TOKENIZER
        .get_or_init(|| {
            let mut t = tokenizers::Tokenizer::from_bytes(include_bytes!("ettin-tokenizer.json"))
                .map_err(|e| e.to_string())?;
            t.with_padding(None);
            t.with_truncation(None).map_err(|e| e.to_string())?;
            Ok(t)
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

#[derive(Debug)]
pub struct HistoryEvidence {
    pub evidence: Value,
    pub history_tokens: usize,
    pub total_tokens: usize,
    pub base_tokens: usize,
    pub original_snippets: usize,
    pub retained: Vec<Span>,
    pub removed: Vec<(usize, &'static str)>,
    pub over_limit: bool,
}
/// Base evidence is already bounded/redacted exactly once; this only budgets history.
pub fn evidence(original_item: &Value, text: &str, base: &Value) -> Result<HistoryEvidence> {
    evidence_from_spans(base, retrieve(original_item, text, &base["session"]))
}
/// Budgets already retrieved spans into `base`; the transcript is used only through them.
pub fn evidence_from_spans(base: &Value, mut work: Vec<Span>) -> Result<HistoryEvidence> {
    let tokenizer = history_tokenizer()?;
    let count = |a: &str, b: Option<&str>, special| -> Result<usize> {
        let input = match b {
            Some(b) => tokenizers_input(a, b),
            None => a.into(),
        };
        Ok(tokenizer
            .encode(input, special)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .len())
    };
    let original_snippets = work.len();
    for s in &mut work {
        s.snippet.time = s.snippet.time.as_deref().map(redact);
    }
    let mut session = indexmap::IndexMap::new();
    let source = base["session"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("session must be object"))?;
    for k in ["conversation", "files", "references", "mentions"] {
        if let Some(v) = source.get(k) {
            session.insert(k.into(), v.clone());
        }
    }
    let mut extra = source
        .keys()
        .filter(|k| !session.contains_key(*k) && *k != "history")
        .collect::<Vec<_>>();
    extra.sort();
    for k in extra {
        session.insert(k.clone(), source[k].clone());
    }
    let item = base["item"].to_string();
    let base_tokens = count(
        &item,
        Some(&Value::Object(session.clone()).to_string()),
        true,
    )?;
    let mut removed = Vec::new();
    loop {
        let history = serde_json::from_str::<Value>(&serde_json::to_string(
            &work.iter().map(|s| &s.snippet).collect::<Vec<_>>(),
        )?)?;
        let ht = count(&history.to_string(), None, false)?;
        session.insert("history".into(), history);
        let total = count(
            &item,
            Some(&Value::Object(session.clone()).to_string()),
            true,
        )?;
        if work.is_empty() || (ht <= 800 && total <= 7999) {
            return Ok(HistoryEvidence {
                evidence: Value::Object(indexmap::IndexMap::from([
                    ("item".into(), base["item"].clone()),
                    ("session".into(), Value::Object(session)),
                ])),
                history_tokens: ht,
                total_tokens: total,
                base_tokens,
                original_snippets,
                retained: work,
                removed,
                over_limit: total > 7999,
            });
        }
        let worst = (0..work.len())
            .min_by(|&a, &b| {
                work[a]
                    .score
                    .total_cmp(&work[b].score)
                    .then(work[a].start.cmp(&work[b].start))
            })
            .unwrap();
        removed.push((
            work[worst].start,
            if ht > 800 {
                "history_budget"
            } else {
                "input_capacity"
            },
        ));
        work.remove(worst);
    }
}
fn tokenizers_input<'a>(a: &'a str, b: &'a str) -> tokenizers::EncodeInput<'a> {
    (a, b).into()
}

/// Build classifier-ready evidence from the spans a scanner retrieved.
/// Diagnostics retain overflow rather than trimming base.
pub fn build_evidence(
    base: &mj_core::github_item::GithubItemEvidence,
    spans: Vec<Span>,
) -> Result<(mj_core::github_item::GithubItemEvidence, HistoryEvidence)> {
    let base = serde_json::from_str(&serde_json::to_string(base)?)?;
    let result = evidence_from_spans(&base, spans)?;
    let typed = serde_json::from_str(&result.evidence.to_string())?;
    Ok((typed, result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_transcript::github_history::student_segments;

    #[test]
    fn github_history_python_format_fixtures() {
        for line in include_str!("../tests/fixtures/github-history/reference.jsonl").lines() {
            let f: Value = serde_json::from_str(line).unwrap();
            let id = &f["pid"];
            let raw = retrieve(
                &f["item"],
                f["transcript_text"].as_str().unwrap(),
                &f["bounded_evidence_without_history"]["session"],
            );
            assert_eq!(
                serde_json::to_string(&raw.iter().map(|s| &s.snippet).collect::<Vec<_>>()).unwrap(),
                f["expected_retrieved_json"].as_str().unwrap(),
                "lexical {id}"
            );
            let prepared: mj_core::github_item::GithubItemEvidence =
                serde_json::from_str(f["expected_evidence_json"].as_str().unwrap()).unwrap();
            let (item_segment, session_segment) = student_segments(&prepared).unwrap();
            assert_eq!(
                item_segment,
                f["expected_item_segment"].as_str().unwrap(),
                "item segment {id}"
            );
            assert_eq!(
                session_segment,
                f["expected_session_segment"].as_str().unwrap(),
                "session segment {id}"
            );
            for (span, expected) in raw.iter().zip(f["expected_raw_spans"].as_array().unwrap()) {
                assert_eq!(span.start as u64, expected["start"].as_u64().unwrap());
                assert_eq!(span.end as u64, expected["end"].as_u64().unwrap());
                assert_eq!(
                    format!("{:016x}", span.score.to_bits()),
                    expected["score_bits"].as_str().unwrap(),
                    "score {id}"
                );
            }
            let actual = evidence(
                &f["item"],
                f["transcript_text"].as_str().unwrap(),
                &f["bounded_evidence_without_history"],
            )
            .unwrap();
            assert_eq!(
                actual.evidence["session"]["history"].to_string(),
                f["expected_history_json"].as_str().unwrap(),
                "budget {id}"
            );
            assert_eq!(
                actual.evidence.to_string(),
                f["expected_evidence_json"].as_str().unwrap(),
                "evidence {id}"
            );
            assert_eq!(
                actual.history_tokens as u64,
                f["expected_stats"]["history_tokens"].as_u64().unwrap(),
                "tokens {id}"
            );
            assert_eq!(
                actual.total_tokens as u64,
                f["expected_stats"]["total_tokens"].as_u64().unwrap(),
                "pair tokens {id}"
            );
        }
    }
}

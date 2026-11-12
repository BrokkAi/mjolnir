//! Byte-exact aliases600 retrieval and evidence envelope from the frozen Python format.
use anyhow::Result;
use mj_core::github_item::history_unicode as u;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;
/// Local ordered JSON, so the Python format never changes global serde_json features.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OrderedValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<OrderedValue>),
    Object(indexmap::IndexMap<String, OrderedValue>),
}
use OrderedValue as Value;
impl Value {
    pub fn as_str(&self) -> Option<&str> {
        if let Self::String(s) = self {
            Some(s)
        } else {
            None
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        if let Self::Number(n) = self {
            n.as_u64()
        } else {
            None
        }
    }
    pub fn as_object(&self) -> Option<&indexmap::IndexMap<String, Value>> {
        if let Self::Object(o) = self {
            Some(o)
        } else {
            None
        }
    }
    pub fn as_array(&self) -> Option<&Vec<Value>> {
        if let Self::Array(a) = self {
            Some(a)
        } else {
            None
        }
    }
}
impl std::ops::Index<&str> for Value {
    type Output = Self;
    fn index(&self, k: &str) -> &Self {
        self.as_object()
            .and_then(|o| o.get(k))
            .unwrap_or(&Self::Null)
    }
}
impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&serde_json::to_string(self).map_err(|_| std::fmt::Error)?)
    }
}
impl PartialEq<&str> for Value {
    fn eq(&self, s: &&str) -> bool {
        self.as_str() == Some(*s)
    }
}

use std::collections::{BTreeMap, HashSet};

fn re(s: &str) -> Regex {
    Regex::new(s).expect("frozen expression")
}
fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}
fn slice(s: &[char], a: usize, b: usize) -> String {
    s[a..b].iter().collect()
}
pub fn redact(s: &str) -> String {
    re(r"[A-Za-z0-9_.+\-]{24,}")
        .replace_all(s, "[redacted]")
        .into_owned()
}
/// Python json.dumps with its default separators, retaining source object order.
fn spaced(v: &Value) -> String {
    match v {
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, v)| format!("{}: {}", json!(k), spaced(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Array(a) => format!("[{}]", a.iter().map(spaced).collect::<Vec<_>>().join(", ")),
        _ => v.to_string(),
    }
}
pub use mj_core::github_item::HistorySnippet as Snippet;
#[derive(Clone, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub score: f64,
    pub snippet: Snippet,
}
fn identifiers(item: &Value) -> Vec<(String, f64)> {
    let title = item["title"].as_str().unwrap_or("");
    let text = format!("{}\n{}", title, item["body"].as_str().unwrap_or(""));
    let word = u::class(u::WORD);
    let decimal = u::class(u::DECIMAL);
    let boundary = format!("(?:$|[^{w}])", w = &word[1..word.len() - 1]);
    let mut ids = Vec::<(String, f64)>::new();
    let mut put = |s: &str, w| {
        if let Some(x) = ids.iter_mut().find(|x| x.0 == s) {
            x.1 = w;
        } else if !s.is_empty() {
            ids.push((s.into(), w));
        }
    };
    // Boundaries are checked separately so a rejected match still advances as Python does.
    for m in re(&format!("#({decimal}+)")).captures_iter(&text) {
        let end = m.get(0).unwrap().end();
        if text[end..]
            .chars()
            .next()
            .is_none_or(|c| !u::contains(u::WORD, c))
        {
            put(&m[1], 1.3);
        }
    }
    for m in re(&format!(
        r"[{w}.-]+(?:/[{w}.:-]+)+",
        w = &word[1..word.len() - 1]
    ))
    .find_iter(&text)
    {
        let before = chars(&text[..m.start()]);
        let tail = chars(&text[m.start()..]);
        let nearby = format!(
            "{}{}",
            slice(&before, before.len().saturating_sub(8), before.len()),
            slice(&tail, 0, 3.min(tail.len()))
        );
        if nearby.contains("://") || re("[a-f0-9]{16,}").is_match(m.as_str()) {
            continue;
        }
        put(m.as_str(), 1.2);
        put(m.as_str().rsplit('/').next().unwrap(), 1.0);
    }
    for m in re(r"[A-Za-z_][A-Za-z_0-9]*(?:\.[A-Za-z_][A-Za-z_0-9]*)*").find_iter(&text) {
        let a = m.as_str();
        if re("^[a-fA-F0-9]{7,}$").is_match(a) {
            continue;
        }
        for s in std::iter::once(a).chain(a.split('.')) {
            if s.len() >= 6 && (s.contains('_') || re("[a-z][A-Z]").is_match(s)) {
                put(s, 1.0);
            }
            let bits = s.split('_').collect::<Vec<_>>();
            if bits.len() > 2 {
                let p = bits[..2].join("_");
                if p.len() >= 6 {
                    put(&p, 0.7);
                }
            }
        }
    }
    for (source, pattern, weight) in [
        (
            &text,
            format!(
                r"[A-Za-z_][{w}-]*\.(?:rs|py|sql|json|tsv)",
                w = &word[1..word.len() - 1]
            ),
            1.0,
        ),
        (&title.to_string(), "[A-Z]{3,}".into(), 0.5),
    ] {
        let pattern = re(&pattern);
        let right = re(&format!("^{boundary}"));
        let mut offset = 0;
        while let Some(m) = pattern.find_at(source, offset) {
            if source[..m.start()]
                .chars()
                .next_back()
                .is_none_or(|c| !u::contains(u::WORD, c))
                && right.is_match(&source[m.end()..])
            {
                put(m.as_str(), weight);
                offset = m.end();
            } else {
                // Python's boundary is inside the regex: a rejected start must
                // not consume a later valid filename within this candidate.
                offset = m.start() + source[m.start()..].chars().next().unwrap().len_utf8();
            }
        }
    }
    ids
}
/// Original title/body must be supplied, independently of the bounded classifier item.
pub fn retrieve(item: &Value, text: &str, session: &Value) -> Vec<Span> {
    let t = chars(text);
    let byte_positions = text
        .char_indices()
        .map(|(byte, _)| byte)
        .collect::<Vec<_>>();
    let digest = chars(&spaced(session));
    let known: HashSet<String> = (0..digest.len().saturating_sub(99))
        .map(|i| slice(&digest, i, i + 100))
        .collect();
    let existing = session["conversation"].as_str().unwrap_or("");
    let mut candidates: BTreeMap<usize, (f64, usize, HashSet<String>)> = BTreeMap::new();
    for (id, base) in identifiers(item) {
        let numeric = id.chars().all(|c| u::contains(u::ISDIGIT, c));
        let mut hits = Vec::new();
        let mut offset = 0;
        while let Some(p) = text[offset..].find(&id) {
            let b = offset + p;
            let pos = byte_positions.partition_point(|&byte| byte < b);
            let end = pos + id.chars().count();
            offset = b + id.len();
            let ok = if numeric {
                (pos == 0 || !u::contains(u::ISDIGIT, t[pos - 1]))
                    && (end == t.len() || !u::contains(u::ISDIGIT, t[end]))
            } else {
                (pos == 0 || !(u::contains(u::ISALNUM, t[pos - 1]) || t[pos - 1] == '_'))
                    && (end == t.len() || !u::contains(u::ISALNUM, t[end]))
            };
            if ok {
                hits.push(pos);
            }
        }
        let weight =
            base * (1.0 + ((t.len() as f64 / 500.0).max(1.0) / (1 + hits.len()) as f64).ln_1p());
        for pos in hits {
            let start = (pos / 250 * 250).saturating_sub(125);
            let c = candidates.entry(start).or_insert((
                0.0,
                (start + 500).min(t.len()),
                HashSet::new(),
            ));
            if c.2.insert(id.clone()) {
                c.0 += weight;
            }
        }
    }
    let stamps = re(r"(?m)^\[([^\]\n]+)\]")
        .captures_iter(text)
        .map(|m| {
            (
                byte_positions.partition_point(|&byte| byte < m.get(0).unwrap().start()),
                m[1].to_string(),
            )
        })
        .collect::<Vec<_>>();
    let mut ranked = Vec::new();
    for (start, (score, end, _)) in candidates {
        let raw = slice(&t, start, end);
        let raw = raw.trim_matches(|c| u::contains(u::SPACE, c));
        let rt = redact(raw);
        let r = chars(&rt);
        if existing.contains(&rt)
            || (0..r.len().saturating_sub(99))
                .step_by(50)
                .any(|i| known.contains(&slice(&r, i, i + 100)))
        {
            continue;
        }
        let score = score * (1.0 + 0.1 * start as f64 / t.len().max(1) as f64);
        let time = stamps
            .iter()
            .rev()
            .find(|(p, _)| *p <= start)
            .map(|(_, s)| s.clone());
        ranked.push((
            Span {
                start,
                end,
                score,
                snippet: Snippet { time, text: rt },
            },
            raw.chars().count(),
        ));
    }
    ranked.sort_by(|a, b| {
        b.0.score
            .total_cmp(&a.0.score)
            .then(b.0.start.cmp(&a.0.start))
    });
    let mut selected: Vec<Span> = Vec::new();
    let mut used = 0;
    for (span, len) in ranked {
        if used + len > 2100
            || selected
                .iter()
                .any(|s| span.start < s.end && span.end > s.start)
        {
            continue;
        }
        used += len;
        selected.push(span);
    }
    selected.sort_by_key(|s| s.start);
    selected
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
#[cfg(any(test, feature = "github-history"))]
pub fn evidence(original_item: &Value, text: &str, base: &Value) -> Result<HistoryEvidence> {
    let tokenizer = mj_core::github_item::history_tokenizer()?;
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
    let mut work = retrieve(original_item, text, &base["session"]);
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
#[cfg(any(test, feature = "github-history"))]
fn tokenizers_input<'a>(a: &'a str, b: &'a str) -> tokenizers::EncodeInput<'a> {
    (a, b).into()
}

/// Raw persisted rows retain tool-call JSON order and unknown row kinds for cutoff counts.
#[derive(Clone, Debug, Deserialize)]
pub struct HistoryRow {
    pub stable_id: String,
    pub position: i64,
    pub created_at_ms: i64,
    pub last_changed_at_ms: i64,
    pub body: Value,
}
fn timestamp(ms: i64) -> Result<String> {
    Ok(chrono::DateTime::from_timestamp_millis(ms)
        .ok_or_else(|| anyhow::anyhow!("timestamp out of range"))?
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}
/// Render one shared snapshot at the item's creation time; never use edited later bodies.
pub fn render(
    session_id: &str,
    rows: &[HistoryRow],
    cutoff_ms: i64,
    repo: &str,
    number: u64,
) -> Result<String> {
    let mut ordered = rows.iter().collect::<Vec<_>>();
    ordered.sort_by(|a, b| {
        a.position
            .cmp(&b.position)
            .then(a.stable_id.cmp(&b.stable_id))
    });
    let (mut changed, mut later, mut notices) = (0, 0, 0);
    let mut chunks = Vec::new();
    for row in ordered {
        if row.created_at_ms > cutoff_ms {
            later += 1;
            continue;
        }
        if row.last_changed_at_ms > cutoff_ms {
            changed += 1;
            continue;
        }
        let b = &row.body;
        let s = |v: &Value| v.as_str().unwrap_or("").to_string();
        let rendered = match b["kind"].as_str().unwrap_or("") {
            "user" => Some(format!(
                "USER\n{}",
                b["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|x| x["type"] == "text")
                    .map(|x| s(&x["text"]))
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
            "agent" => Some(format!(
                "AGENT\n{}",
                b["chunks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|x| x["content"]["type"] == "text")
                    .map(|x| s(&x["content"]["text"]))
                    .collect::<String>()
            )),
            "tool" => {
                let call = &b["call"];
                Some(format!(
                    "TOOL {}\nTITLE: {}\nCALL: {}",
                    call["kind"]
                        .as_str()
                        .filter(|x| !x.is_empty())
                        .unwrap_or("call")
                        .to_uppercase(),
                    s(&call["title"]),
                    call
                ))
            }
            "message" => {
                let body = &b["event"]["body"];
                let typ = body["type"].as_str().unwrap_or("");
                if (typ == "new_github_item" || typ.starts_with("github_"))
                    && body["number"].as_u64() == Some(number)
                    && body["repo"]
                        .as_str()
                        .is_none_or(|r| r.to_lowercase() == repo.to_lowercase())
                {
                    notices += 1;
                    continue;
                }
                Some(format!(
                    "PEER MESSAGE {}\n{}",
                    s(&body["from"]["title"]),
                    s(&body["text"])
                ))
            }
            _ => None,
        };
        if let Some(text) = rendered {
            chunks.push(format!("[{}] {}", timestamp(row.created_at_ms)?, text));
        }
    }
    Ok(format!("Session transcript as of {} (UTC)\nSession id: {session_id}\nTranscript rows included: {}; withheld because changed after cutoff: {changed}; excluded because created later: {later}; item-specific GitHub mailbox notices removed: {notices}.\nRows come only from Mjolnir materialized transcript records; timestamps are row creation times.\n\n{}\n",timestamp(cutoff_ms)?,chunks.len(),chunks.join("\n\n")).replace("\r\n","\n").replace('\r',"\n"))
}

/// Build classifier-ready evidence from an original item and a historical snapshot.
/// `base` must already be bounded and redacted once; the original item's full body
/// is used only for retrieval. Diagnostics retain overflow rather than trimming base.
#[cfg(any(test, feature = "github-history"))]
pub fn build_evidence(
    original: &mj_core::github_item::GithubItem,
    transcript: &str,
    base: &mj_core::github_item::GithubItemEvidence,
) -> Result<(mj_core::github_item::GithubItemEvidence, HistoryEvidence)> {
    let original = serde_json::from_str(&serde_json::to_string(original)?)?;
    let base = serde_json::from_str(&serde_json::to_string(base)?)?;
    let result = evidence(&original, transcript, &base)?;
    let typed = serde_json::from_str(&result.evidence.to_string())?;
    Ok((typed, result))
}

#[cfg(test)]
mod tests {
    use super::*;
    // Hard-won: #1505: Rust word/digit categories differ from the training Python interpreter.
    #[test]
    fn github_history_python_unicode_boundaries() {
        let f: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/github-history/unicode-adversarial.json"
        ))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&identifiers(&f["item"])).unwrap(),
            f["identifiers"].to_string()
        );
        let raw = retrieve(&f["item"], f["transcript"].as_str().unwrap(), &f["session"]);
        assert_eq!(
            serde_json::to_string(&raw.iter().map(|s| &s.snippet).collect::<Vec<_>>()).unwrap(),
            f["snippets"].to_string()
        );
    }
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

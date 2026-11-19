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
        match self {
            Self::Object(o) => {
                f.write_str("{")?;
                for (i, (key, value)) in o.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{}:{value}", json!(key))?;
                }
                f.write_str("}")
            }
            Self::Array(a) => {
                f.write_str("[")?;
                for (i, value) in a.iter().enumerate() {
                    if i > 0 {
                        f.write_str(",")?;
                    }
                    write!(f, "{value}")?;
                }
                f.write_str("]")
            }
            Self::Number(n) => f.write_str(&python_number(n)),
            _ => f.write_str(&serde_json::to_string(self).map_err(|_| std::fmt::Error)?),
        }
    }
}
// Python repr uses the same shortest round-trip digits, but a different
// scientific-notation threshold, explicit exponent signs and two-digit exponents.
fn python_number(number: &serde_json::Number) -> String {
    let raw = number.to_string();
    if !number.is_f64() {
        return raw;
    }
    let (sign, unsigned) = raw
        .strip_prefix('-')
        .map_or(("", raw.as_str()), |s| ("-", s));
    let (mantissa, exponent) = unsigned.split_once('e').map_or((unsigned, 0), |(m, e)| {
        (m, e.parse::<i32>().expect("JSON exponent"))
    });
    let decimal = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let digits = mantissa.replace('.', "");
    let leading = digits.bytes().take_while(|&c| c == b'0').count();
    if leading == digits.len() {
        return format!("{sign}0.0");
    }
    let exponent = exponent + decimal - leading as i32 - 1;
    let digits = digits[leading..].trim_end_matches('0');
    if !(-4..16).contains(&exponent) {
        let mantissa = if digits.len() == 1 {
            digits.to_string()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        format!(
            "{sign}{mantissa}e{}{:02}",
            if exponent < 0 { "-" } else { "+" },
            exponent.abs()
        )
    } else {
        let position = exponent + 1;
        if position <= 0 {
            format!("{sign}0.{}{digits}", "0".repeat((-position) as usize))
        } else if position as usize >= digits.len() {
            format!(
                "{sign}{digits}{}.0",
                "0".repeat(position as usize - digits.len())
            )
        } else {
            format!(
                "{sign}{}.{}",
                &digits[..position as usize],
                &digits[position as usize..]
            )
        }
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
        let before = text[..m.start()].chars().rev().take(8).collect::<Vec<_>>();
        let nearby = before
            .iter()
            .rev()
            .copied()
            .chain(text[m.start()..].chars().take(3))
            .collect::<String>();
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
/// Receives rendered transcript text piece by piece.
pub trait TextSink {
    fn push_str(&mut self, text: &str);
}
impl TextSink for String {
    fn push_str(&mut self, text: &str) {
        String::push_str(self, text);
    }
}

impl TextSink for HistoryScanner {
    fn push_str(&mut self, text: &str) {
        HistoryScanner::push_str(self, text);
    }
}

/// One identifier's streaming match state.
struct IdState {
    /// Length in code points.
    cps: usize,
    numeric: bool,
    /// End (code point index) of the last raw occurrence: later occurrences must start at or after it.
    last_end: usize,
    /// Occurrences that passed the boundary checks.
    hits: usize,
}

/// A 500-code-point window around at least one hit.
struct Candidate {
    text: String,
    cps: usize,
    /// Identifier indexes, ascending: the weights are summed in this order.
    ids: Vec<usize>,
    time: Option<String>,
}

/// Single-pass form of the frozen retrieval: the transcript is pushed in pieces
/// and only the last few hundred code points and the candidate windows are
/// kept. Positions are code point indexes, as in the Python original.
pub struct HistoryScanner {
    digest: String,
    existing: String,
    ids: Vec<(String, f64)>,
    info: Vec<IdState>,
    automaton: Option<aho_corasick::nfa::contiguous::NFA>,
    state: aho_corasick::automaton::StateID,
    seen: usize,
    /// Holds the code points `[seen - ring.len(), seen)`. Large enough for a
    /// window that starts 374 code points before a hit that ends one
    /// identifier length later, plus the boundary character before the hit.
    ring: std::collections::VecDeque<char>,
    ring_cap: usize,
    /// Raw occurrences ending at `seen`; the next code point decides their boundary.
    pending: Vec<(usize, usize)>,
    candidates: BTreeMap<usize, Candidate>,
    open: Vec<usize>,
    stamps: std::collections::VecDeque<(usize, String)>,
    stamp_open: Option<(usize, String)>,
    /// Candidates that began inside a stamp that was still open.
    awaiting_stamp: Vec<usize>,
    line_start: bool,
}

impl HistoryScanner {
    /// `item` is the original title/body; `session` the bounded session digest.
    pub fn new(item: &Value, session: &Value) -> Self {
        use aho_corasick::automaton::Automaton;
        use aho_corasick::nfa::contiguous::NFA;
        let ids = identifiers(item);
        let info = ids
            .iter()
            .map(|(id, _)| IdState {
                cps: id.chars().count(),
                numeric: id.chars().all(|c| u::contains(u::ISDIGIT, c)),
                last_end: 0,
                hits: 0,
            })
            .collect::<Vec<_>>();
        let ring_cap = info.iter().map(|i| i.cps).max().unwrap_or(0) + 400;
        let automaton = (!ids.is_empty()).then(|| {
            NFA::new(ids.iter().map(|(id, _)| id.as_bytes())).expect("identifier automaton")
        });
        let state = automaton
            .as_ref()
            .map_or(aho_corasick::automaton::StateID::ZERO, |a| {
                a.start_state(aho_corasick::Anchored::No)
                    .expect("start state")
            });
        Self {
            digest: spaced(session),
            existing: session["conversation"].as_str().unwrap_or("").to_string(),
            ids,
            info,
            automaton,
            state,
            seen: 0,
            ring: Default::default(),
            ring_cap,
            pending: Vec::new(),
            candidates: BTreeMap::new(),
            open: Vec::new(),
            stamps: Default::default(),
            stamp_open: None,
            awaiting_stamp: Vec::new(),
            line_start: true,
        }
    }

    fn ring_at(&self, index: usize) -> char {
        self.ring[index - (self.seen - self.ring.len())]
    }

    fn resolve(&mut self, id: usize, pos: usize, next: Option<char>) {
        let prev = (pos > 0).then(|| self.ring_at(pos - 1));
        let ok = if self.info[id].numeric {
            prev.is_none_or(|c| !u::contains(u::ISDIGIT, c))
                && next.is_none_or(|c| !u::contains(u::ISDIGIT, c))
        } else {
            prev.is_none_or(|c| !(u::contains(u::ISALNUM, c) || c == '_'))
                && next.is_none_or(|c| !u::contains(u::ISALNUM, c))
        };
        if !ok {
            return;
        }
        self.info[id].hits += 1;
        let start = (pos / 250 * 250).saturating_sub(125);
        if !self.candidates.contains_key(&start) {
            let stop = self.seen.min(start + 500);
            let base = self.seen - self.ring.len();
            let text: String = self
                .ring
                .range(start - base..stop - base)
                .copied()
                .collect();
            let time = self
                .stamps
                .iter()
                .rev()
                .find(|(p, _)| *p <= start)
                .map(|(_, s)| s.clone());
            if self.stamp_open.as_ref().is_some_and(|(p, _)| *p <= start) {
                self.awaiting_stamp.push(start);
            }
            if stop - start < 500 {
                self.open.push(start);
            }
            self.candidates.insert(
                start,
                Candidate {
                    text,
                    cps: stop - start,
                    ids: Vec::new(),
                    time,
                },
            );
        }
        let ids = &mut self.candidates.get_mut(&start).unwrap().ids;
        if let Err(at) = ids.binary_search(&id) {
            ids.insert(at, id);
        }
    }

    fn step(&mut self, c: char) {
        use aho_corasick::automaton::Automaton;
        let pending = std::mem::take(&mut self.pending);
        for (id, pos) in pending {
            self.resolve(id, pos, Some(c));
        }
        // Stamps: `(?m)^\[([^\]\n]+)\]`.
        if let Some((pos, text)) = &mut self.stamp_open {
            if c == ']' || c == '\n' {
                let (pos, text) = (*pos, std::mem::take(text));
                self.stamp_open = None;
                let awaiting = std::mem::take(&mut self.awaiting_stamp);
                if c == ']' && !text.is_empty() {
                    for start in awaiting {
                        self.candidates.get_mut(&start).unwrap().time = Some(text.clone());
                    }
                    self.stamps.push_back((pos, text));
                    while self.stamps.len() >= 2 && self.stamps[1].0 + self.ring_cap <= self.seen {
                        self.stamps.pop_front();
                    }
                }
            } else {
                text.push(c);
            }
        } else if self.line_start && c == '[' {
            self.stamp_open = Some((self.seen, String::new()));
        }
        self.line_start = c == '\n';
        if self.ring.len() == self.ring_cap {
            self.ring.pop_front();
        }
        self.ring.push_back(c);
        let candidates = &mut self.candidates;
        self.open.retain(|start| {
            let candidate = candidates.get_mut(start).unwrap();
            candidate.text.push(c);
            candidate.cps += 1;
            candidate.cps < 500
        });
        self.seen += 1;
        if let Some(automaton) = &self.automaton {
            let mut buffer = [0; 4];
            for &byte in c.encode_utf8(&mut buffer).as_bytes() {
                self.state = automaton.next_state(aho_corasick::Anchored::No, self.state, byte);
                if automaton.is_match(self.state) {
                    for k in 0..automaton.match_len(self.state) {
                        let id = automaton.match_pattern(self.state, k).as_usize();
                        let info = &mut self.info[id];
                        let pos = self.seen - info.cps;
                        // Per identifier, matches are leftmost and do not overlap,
                        // whether or not the boundary checks accept them.
                        if pos >= info.last_end {
                            info.last_end = self.seen;
                            self.pending.push((id, pos));
                        }
                    }
                }
            }
        }
    }

    pub fn push_str(&mut self, text: &str) {
        if self.automaton.is_none() {
            return;
        }
        for c in text.chars() {
            self.step(c);
        }
    }

    /// Scores, filters and selects the windows once the whole transcript was pushed.
    pub fn finish(mut self) -> Vec<Span> {
        for (id, pos) in std::mem::take(&mut self.pending) {
            self.resolve(id, pos, None);
        }
        let total = self.seen;
        let digest = chars(&self.digest);
        let known: HashSet<String> = (0..digest.len().saturating_sub(99))
            .map(|i| slice(&digest, i, i + 100))
            .collect();
        // `f64::ln_1p` calls the platform's C library, whose log1p is not
        // correctly rounded: glibc, Apple's libm and MSVC each differ by an
        // ulp on some inputs, which broke byte parity on Windows. The pure
        // Rust fdlibm port gives the same bits on every platform and
        // reproduces every reference fixture.
        let weights = self
            .ids
            .iter()
            .zip(&self.info)
            .map(|((_, base), info)| {
                base * (1.0 + libm::log1p((total as f64 / 500.0).max(1.0) / (1 + info.hits) as f64))
            })
            .collect::<Vec<_>>();
        let mut ranked = Vec::new();
        for (start, candidate) in std::mem::take(&mut self.candidates) {
            let score = candidate.ids.iter().fold(0.0, |sum, &id| sum + weights[id]);
            let end = (start + 500).min(total);
            let raw = candidate.text.trim_matches(|c| u::contains(u::SPACE, c));
            let rt = redact(raw);
            let r = chars(&rt);
            if self.existing.contains(&rt)
                || (0..r.len().saturating_sub(99))
                    .step_by(50)
                    .any(|i| known.contains(&slice(&r, i, i + 100)))
            {
                continue;
            }
            let score = score * (1.0 + 0.1 * start as f64 / total.max(1) as f64);
            ranked.push((
                Span {
                    start,
                    end,
                    score,
                    snippet: Snippet {
                        time: candidate.time,
                        text: rt,
                    },
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
}

/// Retrieves from a complete transcript text. Original title/body must be
/// supplied, independently of the bounded classifier item.
pub fn retrieve(item: &Value, text: &str, session: &Value) -> Vec<Span> {
    let mut scanner = HistoryScanner::new(item, session);
    scanner.push_str(text);
    scanner.finish()
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
    evidence_from_spans(base, retrieve(original_item, text, &base["session"]))
}
/// Budgets already retrieved spans into `base`; the transcript is used only through them.
#[cfg(any(test, feature = "github-history"))]
pub fn evidence_from_spans(base: &Value, mut work: Vec<Span>) -> Result<HistoryEvidence> {
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

/// Counts that the transcript header reports; they fix the header's length and
/// so every code point position after it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HistoryCounts {
    pub included: usize,
    pub changed: usize,
    pub later: usize,
    pub notices: usize,
}

/// What the first pass needs from each row. SQLite's `json_extract` yields
/// these cheaply, without decoding the whole body.
pub struct RowMeta<'a> {
    pub created_at_ms: i64,
    pub last_changed_at_ms: i64,
    /// `body.kind`, empty unless a string.
    pub kind: &'a str,
    /// `body.event.body.type`, empty unless a string.
    pub event_type: &'a str,
    /// `body.event.body.number` when an unsigned integer.
    pub event_number: Option<u64>,
    /// `body.event.body.repo` when a string.
    pub event_repo: Option<&'a str>,
}

fn kind_renders(kind: &str) -> bool {
    matches!(kind, "user" | "agent" | "tool" | "message")
}

fn is_notice_type(event_type: &str) -> bool {
    event_type == "new_github_item" || event_type.starts_with("github_")
}

/// Whether a notice of a notice type names this pair's item.
fn notice_names_item(
    number: Option<u64>,
    repo: Option<&str>,
    item_number: u64,
    item_repo_lower: &str,
) -> bool {
    number == Some(item_number) && repo.is_none_or(|r| r.to_lowercase() == item_repo_lower)
}

fn timestamp(ms: i64) -> Result<String> {
    Ok(chrono::DateTime::from_timestamp_millis(ms)
        .ok_or_else(|| anyhow::anyhow!("timestamp out of range"))?
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// One decoded row, rendered once for every pair that includes it.
struct RowRender {
    /// None for kinds the transcript never shows.
    text: Option<String>,
    /// Set for message rows of a notice type: (number, repo) decide per pair.
    notice: Option<(Option<u64>, Option<String>)>,
    created: Result<String, ()>,
}

fn render_row(created_at_ms: i64, b: &Value) -> RowRender {
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    let mut notice = None;
    let text = match b["kind"].as_str().unwrap_or("") {
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
            if typ == "new_github_item" || typ.starts_with("github_") {
                notice = Some((
                    body["number"].as_u64(),
                    body["repo"].as_str().map(str::to_string),
                ));
            }
            Some(format!(
                "PEER MESSAGE {}\n{}",
                s(&body["from"]["title"]),
                s(&body["text"])
            ))
        }
        _ => None,
    };
    RowRender {
        text,
        notice,
        created: timestamp(created_at_ms).map_err(|_| ()),
    }
}

/// `\r\n` and lone `\r` become `\n` across piece boundaries.
#[derive(Default)]
struct LineEndings {
    after_cr: bool,
}
impl LineEndings {
    fn push(&mut self, mut s: &str, sink: &mut impl TextSink) {
        if self.after_cr && !s.is_empty() {
            self.after_cr = false;
            s = s.strip_prefix('\n').unwrap_or(s);
        }
        while let Some(i) = s.find('\r') {
            sink.push_str(&s[..i]);
            sink.push_str("\n");
            s = &s[i + 1..];
            match s.strip_prefix('\n') {
                Some(rest) => s = rest,
                None if s.is_empty() => self.after_cr = true,
                None => {}
            }
        }
        sink.push_str(s);
    }
}

struct PairRender<S> {
    cutoff_ms: i64,
    repo_lower: String,
    number: u64,
    expected: HistoryCounts,
    seen: HistoryCounts,
    endings: LineEndings,
    sink: S,
}

impl<S: TextSink> PairRender<S> {
    fn put(&mut self, s: &str) {
        self.endings.push(s, &mut self.sink);
    }
    fn include(&mut self, row: &RowRender, created_at_ms: i64, changed_ms: i64) -> Result<()> {
        if created_at_ms > self.cutoff_ms || changed_ms > self.cutoff_ms {
            return Ok(());
        }
        let Some(text) = &row.text else {
            return Ok(());
        };
        if let Some((n, repo)) = &row.notice
            && notice_names_item(*n, repo.as_deref(), self.number, &self.repo_lower)
        {
            self.seen.notices += 1;
            return Ok(());
        }
        if self.seen.included > 0 {
            self.put("\n\n");
        }
        self.seen.included += 1;
        let ts = row
            .created
            .as_ref()
            .map_err(|()| anyhow::anyhow!("timestamp out of range"))?;
        self.put("[");
        self.put(ts);
        self.put("] ");
        self.put(text);
        Ok(())
    }
}

/// One item's transcript view of a session: its cutoff, the item it is about,
/// and where its rendered text goes.
pub struct HistoryPairSpec<S> {
    pub cutoff_ms: i64,
    pub repo: String,
    pub number: u64,
    pub sink: S,
}

/// Renders one session's rows for several items at once. Rows are read twice,
/// in the same snapshot: `count_row` over all of them fixes each header, then
/// `row` over them in `(position, stable_id)` order renders each exactly once
/// and hands it to every pair that includes it. The rendered text is
/// byte-identical to the whole-string rendering of the frozen format; it is
/// never held whole.
pub struct SessionHistory<S> {
    session_id: String,
    pairs: Vec<PairRender<S>>,
    started: bool,
}

impl<S: TextSink> SessionHistory<S> {
    pub fn new(session_id: &str, pairs: Vec<HistoryPairSpec<S>>) -> Self {
        Self {
            session_id: session_id.to_string(),
            pairs: pairs
                .into_iter()
                .map(|p| PairRender {
                    cutoff_ms: p.cutoff_ms,
                    repo_lower: p.repo.to_lowercase(),
                    number: p.number,
                    expected: HistoryCounts::default(),
                    seen: HistoryCounts::default(),
                    endings: LineEndings::default(),
                    sink: p.sink,
                })
                .collect(),
            started: false,
        }
    }

    /// First pass.
    pub fn count_row(&mut self, row: &RowMeta<'_>) {
        for pair in &mut self.pairs {
            let counts = &mut pair.expected;
            if row.created_at_ms > pair.cutoff_ms {
                counts.later += 1;
            } else if row.last_changed_at_ms > pair.cutoff_ms {
                counts.changed += 1;
            } else if row.kind == "message" {
                if is_notice_type(row.event_type)
                    && notice_names_item(
                        row.event_number,
                        row.event_repo,
                        pair.number,
                        &pair.repo_lower,
                    )
                {
                    counts.notices += 1;
                } else {
                    counts.included += 1;
                }
            } else if kind_renders(row.kind) {
                counts.included += 1;
            }
        }
    }

    /// Writes every header; call once between the passes.
    pub fn begin(&mut self) -> Result<()> {
        for pair in &mut self.pairs {
            let c = pair.expected;
            let header = format!(
                "Session transcript as of {} (UTC)\nSession id: {}\nTranscript rows included: {}; withheld because changed after cutoff: {}; excluded because created later: {}; item-specific GitHub mailbox notices removed: {}.\nRows come only from Mjolnir materialized transcript records; timestamps are row creation times.\n\n",
                timestamp(pair.cutoff_ms)?,
                self.session_id,
                c.included,
                c.changed,
                c.later,
                c.notices
            );
            pair.put(&header);
        }
        self.started = true;
        Ok(())
    }

    /// Second pass. Rows nobody includes are not decoded.
    pub fn row(
        &mut self,
        created_at_ms: i64,
        last_changed_at_ms: i64,
        body_json: &str,
    ) -> Result<()> {
        anyhow::ensure!(self.started, "history rows rendered before the headers");
        if !self
            .pairs
            .iter()
            .any(|p| created_at_ms <= p.cutoff_ms && last_changed_at_ms <= p.cutoff_ms)
        {
            return Ok(());
        }
        let body: Value = serde_json::from_str(body_json)?;
        let rendered = render_row(created_at_ms, &body);
        drop(body);
        for pair in &mut self.pairs {
            pair.include(&rendered, created_at_ms, last_changed_at_ms)?;
        }
        Ok(())
    }

    /// Ends every transcript and returns the sinks. Fails when the second
    /// pass disagrees with the counts in the headers, which means the rows
    /// changed between the passes or the counting query drifted from the renderer.
    pub fn finish(self) -> Result<Vec<S>> {
        self.pairs
            .into_iter()
            .map(|mut pair| {
                anyhow::ensure!(
                    pair.seen.included == pair.expected.included
                        && pair.seen.notices == pair.expected.notices,
                    "history header counts {:?} disagree with the rendered rows {:?}",
                    pair.expected,
                    pair.seen
                );
                pair.put("\n");
                Ok(pair.sink)
            })
            .collect()
    }
}

/// A scanner for one item against one session. `base` must already be bounded
/// and redacted once; the original item's full body is used only for retrieval.
pub fn history_scanner(
    original: &mj_core::github_item::GithubItem,
    base: &mj_core::github_item::GithubItemEvidence,
) -> Result<HistoryScanner> {
    let original: Value = serde_json::from_str(&serde_json::to_string(original)?)?;
    let base: Value = serde_json::from_str(&serde_json::to_string(base)?)?;
    Ok(HistoryScanner::new(&original, &base["session"]))
}

/// Build classifier-ready evidence from the spans a scanner retrieved.
/// Diagnostics retain overflow rather than trimming base.
#[cfg(any(test, feature = "github-history"))]
pub fn build_evidence(
    base: &mj_core::github_item::GithubItemEvidence,
    spans: Vec<Span>,
) -> Result<(mj_core::github_item::GithubItemEvidence, HistoryEvidence)> {
    let base = serde_json::from_str(&serde_json::to_string(base)?)?;
    let result = evidence_from_spans(&base, spans)?;
    let typed = serde_json::from_str(&result.evidence.to_string())?;
    Ok((typed, result))
}

/// Training-format segments, item first, with history last in the session.
/// Evidence is already bounded/redacted; serialization never bounds it again.
pub fn student_segments(
    evidence: &mj_core::github_item::GithubItemEvidence,
) -> Result<(String, String)> {
    Ok((
        serde_json::to_string(&evidence.item)?,
        serde_json::to_string(&evidence.session)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    // The implementation that streaming replaced, kept only as the oracle for
    // the differential test: whole-text retrieval over `Vec<char>` and the
    // whole-string renderer.
    /// Original title/body must be supplied, independently of the bounded classifier item.
    fn retrieve_reference(item: &Value, text: &str, session: &Value) -> Vec<Span> {
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
            // `f64::ln_1p` calls the platform's C library, whose log1p is not
            // correctly rounded: glibc, Apple's libm and MSVC each differ by an
            // ulp on some inputs, which broke byte parity on Windows. The pure
            // Rust fdlibm port gives the same bits on every platform and
            // reproduces every reference fixture.
            let weight = base
                * (1.0 + libm::log1p((t.len() as f64 / 500.0).max(1.0) / (1 + hits.len()) as f64));
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
    /// Raw persisted rows retain tool-call JSON order and unknown row kinds for cutoff counts.
    #[derive(Clone, Debug, Deserialize)]
    struct HistoryRow {
        pub stable_id: String,
        pub position: i64,
        pub created_at_ms: i64,
        pub last_changed_at_ms: i64,
        pub body: Value,
    }
    /// Render one shared snapshot at the item's creation time; never use edited later bodies.
    fn render_reference(
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

    /// Runs both passes of the streaming renderer over rows held in memory,
    /// the way the store feeds them, into the given sinks.
    fn stream_pairs<S: TextSink>(
        session_id: &str,
        rows: &[HistoryRow],
        pairs: Vec<HistoryPairSpec<S>>,
    ) -> Vec<S> {
        let mut ordered = rows.iter().collect::<Vec<_>>();
        ordered.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then(a.stable_id.cmp(&b.stable_id))
        });
        let bodies = ordered
            .iter()
            .map(|r| serde_json::to_string(&r.body).unwrap())
            .collect::<Vec<_>>();
        let mut history = SessionHistory::new(session_id, pairs);
        for row in &ordered {
            let b = &row.body;
            let event = &b["event"]["body"];
            history.count_row(&RowMeta {
                created_at_ms: row.created_at_ms,
                last_changed_at_ms: row.last_changed_at_ms,
                kind: b["kind"].as_str().unwrap_or(""),
                event_type: event["type"].as_str().unwrap_or(""),
                event_number: event["number"].as_u64(),
                event_repo: event["repo"].as_str(),
            });
        }
        history.begin().unwrap();
        for (row, body) in ordered.iter().zip(&bodies) {
            history
                .row(row.created_at_ms, row.last_changed_at_ms, body)
                .unwrap();
        }
        history.finish().unwrap()
    }
    fn stream_render(
        session_id: &str,
        rows: &[HistoryRow],
        cutoff_ms: i64,
        repo: &str,
        number: u64,
    ) -> String {
        stream_pairs(
            session_id,
            rows,
            vec![HistoryPairSpec {
                cutoff_ms,
                repo: repo.into(),
                number,
                sink: String::new(),
            }],
        )
        .pop()
        .unwrap()
    }
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }
    const PIECES: &[&str] = &[
        "#42",
        "#420",
        "x#42",
        "#42y",
        "#4",
        "42",
        "src/foo_bar.rs",
        "foo_bar.rs",
        "foo_bar",
        "foo_bar_baz_qux",
        "FooBar",
        "fooBar_x",
        "CAFÉ",
        "CAFE",
        "AB",
        "ABC",
        "ABCD",
        "é",
        "😀",
        "日本語",
        "²",
        "٣",
        "_",
        ".",
        "-",
        "/",
        " ",
        " ",
        " ",
        "word",
        "words",
        "\n",
        "\n",
        "\r\n",
        "\r",
        "\r\n\r\n",
        "\n[2024-01-01 10:00]\n",
        "\n[x",
        "]",
        "[]",
        "\n[a\nb]",
        "\u{a0}",
        "\t",
        "tokenaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "https://example.com/src/foo_bar.rs",
        "the quick brown fox jumps over the lazy dog ",
    ];
    fn random_text(rng: &mut Rng, pieces: usize) -> String {
        let mut text = String::new();
        for _ in 0..pieces {
            text.push_str(PIECES[rng.below(PIECES.len())]);
            if rng.below(6) == 0 {
                text.push_str(&"lorem ipsum ".repeat(rng.below(60)));
            }
        }
        text
    }
    fn random_rows(rng: &mut Rng, count: usize) -> Vec<HistoryRow> {
        (0..count)
            .map(|n| {
                let text = if rng.below(40) == 0 {
                    // More than a pipe buffer in one row.
                    format!("{}{}", random_text(rng, 30), "padding ".repeat(9000))
                } else {
                    {
                        let pieces = 1 + rng.below(40);
                        random_text(rng, pieces)
                    }
                };
                let created = 500 + rng.below(1200) as i64;
                let changed = if rng.below(6) == 0 { created + rng.below(900) as i64 } else { created };
                let body = match rng.below(7) {
                    0 => json!({"kind": "user", "content": [{"type": "text", "text": text}, {"type": "image"}, {"type": "text", "text": "#42"}]}),
                    1 => json!({"kind": "agent", "chunks": [{"content": {"type": "text", "text": text}}, {"content": {"type": "text", "text": "src/foo_bar.rs"}}]}),
                    2 => json!({"kind": "tool", "call": {"title": text, "kind": if rng.below(2) == 0 { "execute" } else { "" }, "rawInput": {"n": 1.5e300, "z": 0.1, "t": "#42\r\n"}, "toolCallId": "c"}}),
                    3 | 4 => {
                        let typ = ["github_issue", "new_github_item", "peer", "github_comment"][rng.below(4)];
                        let repo = [json!("Owner/Repo"), json!("other/repo"), json!(null), json!(7)][rng.below(4)].clone();
                        let number = [json!(42), json!(43), json!(42.0), json!("42")][rng.below(4)].clone();
                        json!({"kind": "message", "event": {"body": {"type": typ, "number": number, "repo": repo, "text": text, "from": {"title": "peer"}}}})
                    }
                    5 => json!({"kind": "thought", "text": text}),
                    _ => json!({"kind": 5, "x": text}),
                };
                HistoryRow {
                    stable_id: format!("row-{}", rng.below(count + 3)),
                    position: n as i64 / 2,
                    created_at_ms: created,
                    last_changed_at_ms: changed,
                    body: serde_json::from_value(body).unwrap(),
                }
            })
            .collect()
    }
    fn random_item(rng: &mut Rng) -> Value {
        let title = random_text(rng, 4);
        let body = format!(
            "{} #42 src/foo_bar.rs foo_bar_baz_qux FooBar CAFÉ ABC {}",
            random_text(rng, 8),
            random_text(rng, 8)
        );
        serde_json::from_value(json!({"title": title, "body": body})).unwrap()
    }
    fn spans_json(spans: &[Span]) -> String {
        spans
            .iter()
            .map(|s| {
                format!(
                    "{}:{}:{:016x}:{:?}:{:?}",
                    s.start,
                    s.end,
                    s.score.to_bits(),
                    s.snippet.time,
                    s.snippet.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // Streaming must reproduce the whole-text implementation exactly: header
    // counts, line-ending replacement, notices, rows that changed after the
    // cutoff, multi-byte text, identifiers across the window grid and the row
    // boundaries, bracket lines, and rows larger than a pipe buffer.
    #[test]
    fn streaming_history_matches_the_whole_text_reference() {
        let mut rng = Rng(0x9e3779b97f4a7c15);
        let mut retrieved = 0;
        for round in 0..30 {
            let count = 1 + rng.below(40);
            let rows = random_rows(&mut rng, count);
            let item = random_item(&mut rng);
            let session: Value = serde_json::from_value(json!({
                "conversation": if round % 3 == 0 { "lorem ipsum lorem ipsum" } else { "" },
                "files": [], "references": []
            }))
            .unwrap();
            let pairs = [
                (1200, "owner/repo", 42),
                (900, "OWNER/REPO", 42),
                (700, "owner/repo", 43),
            ];
            let specs = pairs
                .iter()
                .map(|&(cutoff_ms, repo, number)| HistoryPairSpec {
                    cutoff_ms,
                    repo: repo.into(),
                    number,
                    sink: HistoryScanner::new(&item, &session),
                })
                .collect();
            let scanners = stream_pairs("session-é", &rows, specs);
            for (scanner, &(cutoff, repo, number)) in scanners.into_iter().zip(&pairs) {
                let text = render_reference("session-é", &rows, cutoff, repo, number).unwrap();
                let expected = retrieve_reference(&item, &text, &session);
                retrieved += expected.len();
                assert_eq!(
                    spans_json(&scanner.finish()),
                    spans_json(&expected),
                    "round {round} pair {cutoff} {number}"
                );
                assert_eq!(
                    stream_render("session-é", &rows, cutoff, repo, number),
                    text,
                    "round {round} text"
                );
                // Pushing the text in arbitrary pieces changes nothing.
                let mut scanner = HistoryScanner::new(&item, &session);
                let mut rest = text.as_str();
                while !rest.is_empty() {
                    let mut cut = (1 + rng.below(700)).min(rest.len());
                    while !rest.is_char_boundary(cut) {
                        cut += 1;
                    }
                    scanner.push_str(&rest[..cut]);
                    rest = &rest[cut..];
                }
                assert_eq!(
                    spans_json(&scanner.finish()),
                    spans_json(&expected),
                    "round {round} pieces"
                );
            }
        }
        assert!(
            retrieved > 100,
            "the generator must produce retrievals; got {retrieved}"
        );
    }

    // Hard-won: #1505: JSON float exponents and notation differed from Python tool rendering.
    #[test]
    fn github_history_python_tool_numbers() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/github-history/tool-numbers.json"
        ))
        .unwrap();
        let row = HistoryRow {
            stable_id: "tool".into(),
            position: 1,
            created_at_ms: 1000,
            last_changed_at_ms: 1000,
            body: fixture["body"].clone(),
        };
        assert_eq!(
            stream_render("floats", &[row], 2000, "owner/repo", 42),
            fixture["expected"].as_str().unwrap()
        );
    }
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

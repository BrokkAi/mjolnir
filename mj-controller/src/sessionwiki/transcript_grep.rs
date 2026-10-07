//! Serves Mjolnir preview passages and history tools; this is not part of upstream SessionWiki.

use chrono::{DateTime, Utc};
use sessionwiki::model::{Role, Session};

/// How much of a session a transcript search returns.
#[derive(Debug, Clone)]
pub(super) struct GrepOpts {
    /// Messages of context kept on each side of a matching message.
    pub context_messages: usize,
    /// Characters kept per message, windowed around its first match with a
    /// quarter of the budget as lead-in. Zero keeps the whole message.
    pub chars: usize,
    /// Stop after this many matching messages. `None` returns all of them.
    pub max_matches: Option<usize>,
    /// Roles whose messages may anchor a hit. Other roles may still appear as
    /// context around a hit.
    pub anchor_roles: Vec<Role>,
}

impl Default for GrepOpts {
    fn default() -> Self {
        Self {
            context_messages: 0,
            chars: 0,
            max_matches: None,
            anchor_roles: vec![Role::User, Role::Assistant, Role::Tool],
        }
    }
}

/// One message returned by a transcript search.
#[derive(Debug, Clone)]
pub(super) struct GrepHit {
    pub i: usize,
    pub role: Role,
    pub ts: Option<DateTime<Utc>>,
    /// Redacted, NFC-normalized, trimmed, and optionally windowed message text.
    pub text: String,
    /// Byte ranges of matches in `text`; empty for context messages.
    pub matches: Vec<(usize, usize)>,
    pub truncated: bool,
    /// Number of messages skipped before this passage group.
    pub omitted_before: usize,
}

#[derive(Debug, Clone, Default)]
pub(super) struct GrepResult {
    pub hits: Vec<GrepHit>,
    pub omitted_after: usize,
}

/// Return matching messages and their surrounding context from `session`.
pub(super) fn grep_session(session: &Session, pattern: &str, opts: &GrepOpts) -> GrepResult {
    let needle = sessionwiki::util::nfc(pattern.trim()).to_lowercase();
    if needle.is_empty() || session.messages.is_empty() {
        return GrepResult::default();
    }
    let texts: Vec<String> = session
        .messages
        .iter()
        .map(|message| {
            sessionwiki::redact::redact(&sessionwiki::util::nfc(message.text.trim())).into_owned()
        })
        .collect();
    let found: Vec<Vec<(usize, usize)>> = texts
        .iter()
        .zip(&session.messages)
        .map(|(text, message)| {
            if opts.anchor_roles.contains(&message.role) {
                matches_in(text, &needle)
            } else {
                Vec::new()
            }
        })
        .collect();

    let last = texts.len() - 1;
    let anchors = (0..texts.len())
        .filter(|index| !found[*index].is_empty())
        .take(opts.max_matches.unwrap_or(usize::MAX));
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for index in anchors {
        let start = index.saturating_sub(opts.context_messages);
        let end = (index + opts.context_messages).min(last);
        match groups.last_mut() {
            Some(previous) if start <= previous.1 + 1 => previous.1 = previous.1.max(end),
            _ => groups.push((start, end)),
        }
    }
    if groups.is_empty() {
        return GrepResult::default();
    }

    let mut hits = Vec::new();
    let mut previous_end = None;
    for (start, end) in &groups {
        let omitted = match previous_end {
            Some(previous) => start - previous - 1,
            None => *start,
        };
        for index in *start..=*end {
            let (text, matches, truncated) = excerpt(&texts[index], &found[index], opts.chars);
            hits.push(GrepHit {
                i: index,
                role: session.messages[index].role,
                ts: session.messages[index].ts,
                text,
                matches,
                truncated,
                omitted_before: if index == *start { omitted } else { 0 },
            });
        }
        previous_end = Some(*end);
    }
    GrepResult {
        hits,
        omitted_after: last - previous_end.unwrap_or(last),
    }
}

/// Byte ranges of non-overlapping case-insensitive occurrences of `needle`.
fn matches_in(text: &str, needle: &str) -> Vec<(usize, usize)> {
    let mut lowered = String::with_capacity(text.len());
    let mut origin: Vec<usize> = Vec::with_capacity(text.len() + 1);
    for (index, character) in text.char_indices() {
        let before = lowered.len();
        lowered.extend(character.to_lowercase());
        origin.resize(origin.len() + (lowered.len() - before), index);
    }
    origin.push(text.len());

    let mut hits = Vec::new();
    let mut from = 0;
    while let Some(offset) = lowered[from..].find(needle) {
        let start = from + offset;
        from = start + needle.len();
        let begin = origin[start];
        let mut end = origin[from];
        if end <= begin {
            end = text[begin..]
                .chars()
                .next()
                .map_or(begin, |character| begin + character.len_utf8());
        }
        hits.push((begin, end));
    }
    hits
}

/// Cap one message around its first match and rebase the match ranges.
fn excerpt(
    text: &str,
    hits: &[(usize, usize)],
    chars: usize,
) -> (String, Vec<(usize, usize)>, bool) {
    let total = text.chars().count();
    if chars == 0 || total <= chars {
        return (text.to_owned(), hits.to_vec(), false);
    }
    let first = hits
        .first()
        .map_or(0, |(start, _)| text[..*start].chars().count());
    let window_start = first.saturating_sub(chars / 4).min(total - chars);
    let begin = byte_of_char(text, window_start);
    let end = byte_of_char(text, window_start + chars);
    let kept = hits
        .iter()
        .filter_map(|(start, stop)| {
            let start = (*start).max(begin);
            let stop = (*stop).min(end);
            (start < stop).then_some((start - begin, stop - begin))
        })
        .collect();
    (text[begin..end].to_owned(), kept, true)
}

fn byte_of_char(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(offset, _)| offset)
}

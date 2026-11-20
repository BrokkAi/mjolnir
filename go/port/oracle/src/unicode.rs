//! Oracle module `unicode`: the crates `unicode-width` and
//! `unicode-segmentation`, for go/crates/unicodewidth and
//! go/crates/unicodesegmentation.
//!
//! Requests are objects with an `"op"` member:
//!
//! - `char_width_range {start, end}`: `UnicodeWidthChar::width` and
//!   `width_cjk` of every scalar value in `start..end`, and the two string
//!   widths of the one-character string, each as runs `[first code point,
//!   value]` (a width of `None` is -1).
//! - `str_width {strings}`: `UnicodeWidthStr::width` and `width_cjk` of each
//!   string.
//! - `grapheme_probe_range {start, end}`, `word_probe_range {start, end}`:
//!   for every scalar value in the range, where the boundaries fall in a fixed
//!   probe string built around it (see `GRAPHEME_PROBES`, `WORD_PROBES`), as
//!   runs of equal answers. The probes tell every property the segmentation
//!   tables hold for a character apart, so two implementations that agree on
//!   every code point have the same tables.
//! - `segment {strings}`: graphemes (extended and legacy), word bounds and
//!   Unicode words of each string, forwards, backwards and from both ends.
//! - `cursor {s, extended, offset, bounds, ops}`: a `GraphemeCursor` driven
//!   over the string in chunks, the way a rope would drive it.
//!
//! The Go tests in those two packages hold the other side of each operation.

use serde_json::{Map, Value, json};
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete, UnicodeSegmentation};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "char_width_range" => char_width_range(request),
        "str_width" => str_width(request),
        "grapheme_probe_range" => grapheme_probe_range(request),
        "word_probe_range" => word_probe_range(request),
        "segment" => segment(request),
        "cursor" => cursor(request),
        _ => crate::unknown(request),
    }
}

fn number(request: &Value, key: &str) -> u64 {
    request.get(key).and_then(Value::as_u64).unwrap_or_default()
}

fn strings(request: &Value) -> Vec<&str> {
    request
        .get("strings")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// The scalar values in `start..end` of the request.
fn scalar_values(request: &Value) -> impl Iterator<Item = char> {
    let start = number(request, "start") as u32;
    let end = number(request, "end") as u32;
    (start..end).filter_map(char::from_u32)
}

/// Run-length encodes one answer for each character: `[first code point,
/// answer]` wherever the answer changes.
struct Runs {
    runs: Vec<Value>,
    last: Option<Value>,
}

impl Runs {
    fn new() -> Self {
        Runs {
            runs: Vec::new(),
            last: None,
        }
    }

    fn push(&mut self, c: char, answer: Value) {
        if self.last.as_ref() != Some(&answer) {
            self.runs.push(json!([c as u32, answer]));
            self.last = Some(answer);
        }
    }

    fn finish(self) -> Value {
        Value::Array(self.runs)
    }
}

fn option_width(width: Option<usize>) -> Value {
    match width {
        Some(width) => json!(width),
        None => json!(-1),
    }
}

fn char_width_range(request: &Value) -> Value {
    let mut width = Runs::new();
    let mut width_cjk = Runs::new();
    let mut str_width = Runs::new();
    let mut str_width_cjk = Runs::new();
    let mut text = String::new();
    for c in scalar_values(request) {
        width.push(c, option_width(UnicodeWidthChar::width(c)));
        width_cjk.push(c, option_width(UnicodeWidthChar::width_cjk(c)));
        text.clear();
        text.push(c);
        str_width.push(c, json!(UnicodeWidthStr::width(text.as_str())));
        str_width_cjk.push(c, json!(UnicodeWidthStr::width_cjk(text.as_str())));
    }
    json!({
        "width": width.finish(),
        "width_cjk": width_cjk.finish(),
        "str_width": str_width.finish(),
        "str_width_cjk": str_width_cjk.finish(),
    })
}

fn str_width(request: &Value) -> Value {
    let strings = strings(request);
    let width: Vec<usize> = strings.iter().map(|s| UnicodeWidthStr::width(*s)).collect();
    let width_cjk: Vec<usize> = strings.iter().map(|s| UnicodeWidthStr::width_cjk(*s)).collect();
    json!({ "w": width, "c": width_cjk })
}

/// Stands for the character under test in a probe.
const C: u32 = u32::MAX;

/// The probes of `grapheme_probe_range`. Each is a short string in which the
/// character under test (`C`) meets a representative of one grapheme
/// category; the probes are joined by U+0001, a Control character, which
/// breaks on both sides whatever it is next to (GB4, GB5). The Go test keeps
/// the same list.
const GRAPHEME_PROBES: &[&[u32]] = &[
    &[0x0D, C],                 // CR x LF
    &[C, 0x0A],                 // CR x LF
    &[0x61, C],                 // x (Extend | ZWJ | SpacingMark)
    &[C, 0x61],                 // Prepend x
    &[C, 0x300],                // Control / (Extend)
    &[0x1100, C],               // L x (L | V | LV | LVT)
    &[C, 0x11A8],               // (LV | V | LVT | T) x T
    &[0xAC00, C],               // LV x (V | T)
    &[0xAC01, C],               // LVT x T
    &[C, 0x1160],               // (L | LV | V) x V
    &[0x1F1E6, C],              // RI x RI
    &[0x1F352, 0x200D, C],      // ExtPict ZWJ x ExtPict
    &[0x1F352, C, 0x1F951],     // ExtPict (Extend* ZWJ) x ExtPict
    &[0x915, 0x94D, C],         // Consonant Linker x Consonant
    &[0x915, C, 0x915],         // Consonant (Linker) x Consonant
    &[0x915, 0x94D, C, 0x915],  // Consonant Linker (Extend) x Consonant
    &[C, 0x200D, 0x1F352],      // (ExtPict) ZWJ x ExtPict
    &[C, 0x94D, 0x915],         // (Consonant) Linker x Consonant
    &[C, C],
];

/// The probes that are segmented into legacy clusters as well: SpacingMark
/// and Prepend behave differently there, and GB9c does not apply.
const GRAPHEME_LEGACY_PROBES: &[&[u32]] = &[
    &[0x61, C],            // x SpacingMark (GB9a)
    &[C, 0x61],            // Prepend x (GB9b)
    &[0x915, 0x94D, C],    // GB9c
];

const GRAPHEME_PROBE_SEPARATOR: char = '\u{1}';

/// The probes of `word_probe_range`, joined by U+000A: there is a break
/// before and after a line feed (WB3a, WB3b) except in CR LF.
const WORD_PROBES: &[&[u32]] = &[
    &[0x61, C, 0x61],        // AHLetter (MidLetter | MidNumLetQ) AHLetter
    &[0x31, C, 0x31],        // Numeric (MidNum | MidNumLetQ) Numeric
    &[0x5D0, C, 0x5D0],      // Hebrew_Letter Double_Quote Hebrew_Letter
    &[0x5D0, C],             // Hebrew_Letter x Single_Quote
    &[0x30A2, C],            // Katakana x Katakana
    &[C, 0x30A2],
    &[0x5F, C],              // ExtendNumLet x (AHLetter | Numeric | Katakana)
    &[C, 0x5F],
    &[0x1F1E6, C],           // RI x RI
    &[0x0D, C],              // CR x LF
    &[0x20, C],              // WSegSpace x WSegSpace
    &[0x200D, C],            // ZWJ x ExtPict
    &[0x21, C],              // x (Extend | Format | ZWJ)
    &[C, 0x300],             // (Newline | CR | LF) /
    &[C, 0x22, 0x5D0],       // (Hebrew_Letter) Double_Quote Hebrew_Letter
    &[C, 0x2C, 0x31],        // (Numeric) MidNum Numeric
    &[C, 0x3A, 0x61],        // (AHLetter) MidLetter AHLetter
    &[0x21, C, 0x1F352],     // (ZWJ) x ExtPict
    &[C, C],
    &[C],
];

const WORD_PROBE_SEPARATOR: char = '\n';

fn probe_string(probes: &[&[u32]], separator: char, c: char, out: &mut String) {
    out.clear();
    for (index, probe) in probes.iter().enumerate() {
        if index > 0 {
            out.push(separator);
        }
        for &code in probe.iter() {
            if code == C {
                out.push(c);
            } else {
                out.push(char::from_u32(code).expect("probe character"));
            }
        }
    }
}

/// Which characters of `text` start a piece, as a string of hexadecimal
/// digits, four characters to a digit, the first character in the highest bit.
/// `starts` holds the byte offset of each piece.
fn start_mask(text: &str, starts: impl Iterator<Item = usize>) -> String {
    let starts: std::collections::BTreeSet<usize> = starts.collect();
    let mut mask = String::new();
    let mut digit = 0u32;
    let mut bits = 0;
    for (offset, _) in text.char_indices() {
        digit = (digit << 1) | u32::from(starts.contains(&offset));
        bits += 1;
        if bits == 4 {
            mask.push(char::from_digit(digit, 16).expect("hex digit"));
            digit = 0;
            bits = 0;
        }
    }
    if bits > 0 {
        digit <<= 4 - bits;
        mask.push(char::from_digit(digit, 16).expect("hex digit"));
    }
    mask
}

fn grapheme_probe_range(request: &Value) -> Value {
    let mut runs = Runs::new();
    let mut text = String::new();
    let mut legacy_text = String::new();
    for c in scalar_values(request) {
        probe_string(GRAPHEME_PROBES, GRAPHEME_PROBE_SEPARATOR, c, &mut text);
        probe_string(GRAPHEME_LEGACY_PROBES, GRAPHEME_PROBE_SEPARATOR, c, &mut legacy_text);
        let extended = start_mask(&text, text.grapheme_indices(true).map(|(offset, _)| offset));
        let legacy = start_mask(
            &legacy_text,
            legacy_text.grapheme_indices(false).map(|(offset, _)| offset),
        );
        runs.push(c, json!([extended, legacy]));
    }
    json!({ "runs": runs.finish() })
}

fn word_probe_range(request: &Value) -> Value {
    let mut runs = Runs::new();
    let mut text = String::new();
    for c in scalar_values(request) {
        probe_string(WORD_PROBES, WORD_PROBE_SEPARATOR, c, &mut text);
        let bounds = start_mask(&text, text.split_word_bound_indices().map(|(offset, _)| offset));
        let words = start_mask(&text, text.unicode_word_indices().map(|(offset, _)| offset));
        runs.push(c, json!([bounds, words]));
    }
    json!({ "runs": runs.finish() })
}

type Piece<'a> = (usize, &'a str);

fn lengths(pieces: &[Piece]) -> Value {
    Value::Array(pieces.iter().map(|(_, piece)| json!(piece.len())).collect())
}

fn offsets_and_lengths(pieces: &[Piece]) -> Value {
    Value::Array(
        pieces
            .iter()
            .flat_map(|(offset, piece)| [json!(offset), json!(piece.len())])
            .collect(),
    )
}

/// Whether the pieces follow one another from offset 0, as the pieces of a
/// split of the whole string do.
fn contiguous(pieces: &[Piece]) -> bool {
    let mut offset = 0;
    for (start, piece) in pieces {
        if *start != offset {
            return false;
        }
        offset += piece.len();
    }
    true
}

/// Takes pieces from both ends of a double-ended iterator in turn, the front
/// first, and returns them in the order taken, each with the end it came from.
fn from_both_ends<'a>(mut iter: impl DoubleEndedIterator<Item = Piece<'a>>) -> Vec<(bool, Piece<'a>)> {
    let mut taken = Vec::new();
    let mut front = true;
    loop {
        let next = if front { iter.next() } else { iter.next_back() };
        match next {
            Some(piece) => taken.push((front, piece)),
            None => return taken,
        }
        front = !front;
    }
}

fn both_ends_value(taken: &[(bool, Piece)]) -> Value {
    Value::Array(
        taken
            .iter()
            .flat_map(|(front, (offset, piece))| [json!(u8::from(!*front)), json!(offset), json!(piece.len())])
            .collect(),
    )
}

/// Records what an iterator over a split of `s` yields. `key` is the member
/// the forward pieces go under, as lengths; the other directions are added
/// only when they do not yield the same pieces, which keeps a corpus small and
/// still shows any case where they differ.
fn record_split<'a>(
    extras: &mut Map<String, Value>,
    key: &str,
    forward: &[Piece<'a>],
    backward: Vec<Piece<'a>>,
    both: Vec<(bool, Piece<'a>)>,
    plain: Vec<&'a str>,
    plain_backward: Vec<&'a str>,
) {
    if !contiguous(forward) {
        extras.insert(format!("{key}_offsets"), offsets_and_lengths(forward));
    }
    let mut reversed = backward.clone();
    reversed.reverse();
    if reversed != forward {
        extras.insert(format!("{key}_back"), offsets_and_lengths(&backward));
    }
    let mut sorted: Vec<Piece> = both.iter().map(|(_, piece)| *piece).collect();
    sorted.sort();
    if sorted != forward {
        extras.insert(format!("{key}_both"), both_ends_value(&both));
    }
    // The iterator without offsets must yield the same pieces.
    if plain != forward.iter().map(|(_, piece)| *piece).collect::<Vec<_>>() {
        extras.insert(format!("{key}_plain"), json!(plain));
    }
    if plain_backward != backward.iter().map(|(_, piece)| *piece).collect::<Vec<_>>() {
        extras.insert(format!("{key}_plain_back"), json!(plain_backward));
    }
}

fn segment_one(s: &str) -> Value {
    let mut extras = Map::new();

    let extended: Vec<Piece> = s.grapheme_indices(true).collect();
    record_split(
        &mut extras,
        "g",
        &extended,
        s.grapheme_indices(true).rev().collect(),
        from_both_ends(s.grapheme_indices(true)),
        s.graphemes(true).collect(),
        s.graphemes(true).rev().collect(),
    );

    let legacy: Vec<Piece> = s.grapheme_indices(false).collect();
    if legacy != extended {
        extras.insert("l".to_owned(), lengths(&legacy));
    }
    record_split(
        &mut extras,
        "l",
        &legacy,
        s.grapheme_indices(false).rev().collect(),
        from_both_ends(s.grapheme_indices(false)),
        s.graphemes(false).collect(),
        s.graphemes(false).rev().collect(),
    );

    let bounds: Vec<Piece> = s.split_word_bound_indices().collect();
    record_split(
        &mut extras,
        "w",
        &bounds,
        s.split_word_bound_indices().rev().collect(),
        from_both_ends(s.split_word_bound_indices()),
        s.split_word_bounds().collect(),
        s.split_word_bounds().rev().collect(),
    );

    let words: Vec<Piece> = s.unicode_word_indices().collect();
    let words_backward: Vec<Piece> = s.unicode_word_indices().rev().collect();
    let mut reversed = words_backward.clone();
    reversed.reverse();
    if reversed != words {
        extras.insert("u_back".to_owned(), offsets_and_lengths(&words_backward));
    }
    let both = from_both_ends(s.unicode_word_indices());
    let mut sorted: Vec<Piece> = both.iter().map(|(_, piece)| *piece).collect();
    sorted.sort();
    if sorted != words {
        extras.insert("u_both".to_owned(), both_ends_value(&both));
    }
    let plain: Vec<&str> = s.unicode_words().collect();
    if plain != words.iter().map(|(_, piece)| *piece).collect::<Vec<_>>() {
        extras.insert("u_plain".to_owned(), json!(plain));
    }
    let plain_backward: Vec<&str> = s.unicode_words().rev().collect();
    if plain_backward != words_backward.iter().map(|(_, piece)| *piece).collect::<Vec<_>>() {
        extras.insert("u_plain_back".to_owned(), json!(plain_backward));
    }

    let mut reply = vec![lengths(&extended), lengths(&bounds), offsets_and_lengths(&words)];
    if !extras.is_empty() {
        reply.push(Value::Object(extras));
    }
    Value::Array(reply)
}

fn segment(request: &Value) -> Value {
    // A panic while one string is segmented (the crate unwraps what its
    // cursors return) is that string's answer, not the whole request's.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let replies = strings(request)
        .into_iter()
        .map(|s| {
            std::panic::catch_unwind(|| segment_one(s))
                .unwrap_or_else(|payload| json!({ "panic": panic_message(payload) }))
        })
        .collect();
    std::panic::set_hook(hook);
    Value::Array(replies)
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|text| (*text).to_owned()))
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

fn incomplete(error: &GraphemeIncomplete) -> Value {
    match error {
        GraphemeIncomplete::PreContext(offset) => json!(["PreContext", offset]),
        GraphemeIncomplete::PrevChunk => json!("PrevChunk"),
        GraphemeIncomplete::NextChunk => json!("NextChunk"),
        GraphemeIncomplete::InvalidOffset => json!("InvalidOffset"),
    }
}

/// Drives a `GraphemeCursor` over `s` cut into chunks at `bounds`, as the user
/// of a rope does: it hands the cursor one chunk at a time, moves to the
/// neighbouring chunk when asked, and provides the context asked for.
///
/// The operations are `next`, `prev` and `is` (the three queries, on the
/// chunk the driver is at), `next1`, `prev1` and `is1` (the same queries asked
/// once, leaving any request for another chunk or for context unanswered),
/// `seek` (go to the chunk holding the cursor), `set:N` (`set_cursor`),
/// `chunk:K` (go to chunk K, whether or not it holds the cursor) and `ctx:K`
/// (`provide_context` with chunk K, asked for or not). The reply is everything
/// the cursor answered, in order; a panic ends the script and is recorded as
/// the last entry.
fn cursor(request: &Value) -> Value {
    let s = crate::text(request, "s");
    let extended = request.get("extended").and_then(Value::as_bool).unwrap_or(true);
    let offset = number(request, "offset") as usize;
    let mut bounds: Vec<usize> = vec![0];
    if let Some(inner) = request.get("bounds").and_then(Value::as_array) {
        bounds.extend(inner.iter().filter_map(Value::as_u64).map(|bound| bound as usize));
    }
    bounds.push(s.len());
    let chunks: Vec<(usize, &str)> = bounds
        .windows(2)
        .map(|pair| (pair[0], &s[pair[0]..pair[1]]))
        .collect();
    let ops: Vec<&str> = request
        .get("ops")
        .and_then(Value::as_array)
        .map(|ops| ops.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    // The chunk that holds the character after `offset`; the last chunk at the end.
    let chunk_at = |offset: usize| -> usize {
        chunks
            .iter()
            .rposition(|(start, _)| *start <= offset)
            .unwrap_or(0)
    };

    let mut trace: Vec<Value> = Vec::new();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut cursor = GraphemeCursor::new(offset, s.len(), extended);
        let mut at = chunk_at(offset);
        for op in ops {
            if let Some(target) = op.strip_prefix("set:") {
                let target: usize = target.parse().expect("set offset");
                cursor.set_cursor(target);
                trace.push(json!(["set", cursor.cur_cursor()]));
                continue;
            }
            if let Some(target) = op.strip_prefix("chunk:") {
                at = target.parse::<usize>().expect("chunk index") % chunks.len();
                continue;
            }
            if let Some(target) = op.strip_prefix("ctx:") {
                let index = target.parse::<usize>().expect("chunk index") % chunks.len();
                trace.push(json!(["ctx", index]));
                cursor.provide_context(chunks[index].1, chunks[index].0);
                continue;
            }
            if op == "seek" {
                at = chunk_at(cursor.cur_cursor());
                continue;
            }
            // A query is repeated until it is answered, unless it is asked
            // once. Each round either moves to another chunk or provides
            // context, and both are bounded by the number of chunks.
            let (query, once) = match op.strip_suffix('1') {
                Some(query) => (query, true),
                None => (op, false),
            };
            for _ in 0..(4 * chunks.len() + 8) {
                let (start, chunk) = chunks[at];
                let answer: Result<Value, GraphemeIncomplete> = match query {
                    "next" => cursor.next_boundary(chunk, start).map(|found| json!(found)),
                    "prev" => cursor.prev_boundary(chunk, start).map(|found| json!(found)),
                    "is" => cursor.is_boundary(chunk, start).map(|found| json!(found)),
                    _ => panic!("unknown cursor operation {op}"),
                };
                match answer {
                    Ok(found) => {
                        trace.push(json!([op, at, found, cursor.cur_cursor()]));
                        break;
                    }
                    Err(error) => {
                        trace.push(json!([op, at, incomplete(&error), cursor.cur_cursor()]));
                        if once {
                            break;
                        }
                        match error {
                            GraphemeIncomplete::NextChunk if at + 1 < chunks.len() => at += 1,
                            GraphemeIncomplete::PrevChunk if at > 0 => at -= 1,
                            GraphemeIncomplete::PreContext(end) => {
                                match chunks.iter().position(|(start, chunk)| start + chunk.len() == end && !chunk.is_empty()) {
                                    Some(index) => cursor.provide_context(chunks[index].1, chunks[index].0),
                                    None => break,
                                }
                            }
                            _ => break,
                        }
                    }
                }
            }
        }
    }));
    std::panic::set_hook(hook);
    if let Err(payload) = outcome {
        trace.push(json!({ "panic": panic_message(payload) }));
    }
    Value::Array(trace)
}

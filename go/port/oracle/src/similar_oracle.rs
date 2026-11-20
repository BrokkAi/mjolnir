//! Oracle module `similar`. Requests are objects with an `"op"` member; see the
//! Go tests that use this module (`go/crates/similar/oracle_test.go`) for the
//! operations it serves.
//!
//! Text operations (`lines`, `words`, `chars`, `slices`) build a `TextDiff`
//! and reply with everything a caller can observe: the tokens, the ops, the
//! expanded changes, the grouped ops, every hunk, and the unified diff text.
//! With `"digest": true` the reply carries lengths and FNV-1a hashes instead,
//! so that large inputs do not make the recorded corpus large.

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use similar::algorithms::{self, Capture, Compact, DiffHook, IdentifyDistinct, Replace};
use similar::{
    Algorithm, ChangeTag, DiffOp, DiffTag, DiffableStr, TextDiff, capture_diff_slices_deadline,
    get_diff_ratio, group_diff_ops,
};

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "lines" | "words" | "chars" | "slices" => text_diff(request),
        "tokenize" => tokenize(request),
        "capture" => capture(request),
        "raw" => raw(request),
        "identify" => identify(request),
        "group" => group(request),
        _ => crate::unknown(request),
    }
}

fn algorithm(request: &Value) -> Algorithm {
    match crate::text(request, "alg") {
        "patience" => Algorithm::Patience,
        "lcs" => Algorithm::Lcs,
        _ => Algorithm::Myers,
    }
}

/// `"deadline": "passed"` is an instant that has already gone by, which makes
/// every deadline check fail in the same place on every run. `"far"` is one
/// that is never reached.
fn deadline(request: &Value) -> Option<Instant> {
    match crate::text(request, "deadline") {
        "passed" => Some(Instant::now().checked_sub(Duration::from_secs(1)).expect("uptime")),
        "far" => Some(Instant::now() + Duration::from_secs(3600)),
        _ => None,
    }
}

fn strings(request: &Value, key: &str) -> Vec<String> {
    request
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().map(|item| item.as_str().unwrap_or_default().to_owned()).collect())
        .unwrap_or_default()
}

fn numbers(request: &Value, key: &str) -> Vec<u64> {
    request
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().map(|item| item.as_u64().unwrap_or_default()).collect())
        .unwrap_or_default()
}

fn number(request: &Value, key: &str, default: usize) -> usize {
    request.get(key).and_then(Value::as_u64).map_or(default, |value| value as usize)
}

fn op_json(op: &DiffOp) -> Value {
    match *op {
        DiffOp::Equal { old_index, new_index, len } => {
            json!({"op": "equal", "old_index": old_index, "new_index": new_index, "len": len})
        }
        DiffOp::Delete { old_index, old_len, new_index } => {
            json!({"op": "delete", "old_index": old_index, "old_len": old_len, "new_index": new_index})
        }
        DiffOp::Insert { old_index, new_index, new_len } => {
            json!({"op": "insert", "old_index": old_index, "new_index": new_index, "new_len": new_len})
        }
        DiffOp::Replace { old_index, old_len, new_index, new_len } => {
            json!({"op": "replace", "old_index": old_index, "old_len": old_len, "new_index": new_index, "new_len": new_len})
        }
    }
}

fn ops_json(ops: &[DiffOp]) -> Value {
    Value::Array(ops.iter().map(op_json).collect())
}

fn diff_tag(tag: DiffTag) -> &'static str {
    match tag {
        DiffTag::Equal => "equal",
        DiffTag::Delete => "delete",
        DiffTag::Insert => "insert",
        DiffTag::Replace => "replace",
    }
}

/// What `tag()`, `old_range()` and `new_range()` say about each op.
fn tag_tuples_json(ops: &[DiffOp]) -> Value {
    Value::Array(
        ops.iter()
            .map(|op| {
                let (tag, old, new) = op.as_tag_tuple();
                assert_eq!(tag, op.tag());
                assert_eq!(old, op.old_range());
                assert_eq!(new, op.new_range());
                json!([diff_tag(tag), old.start, old.end, new.start, new.end])
            })
            .collect(),
    )
}

fn change_tag(tag: ChangeTag) -> &'static str {
    match tag {
        ChangeTag::Equal => "equal",
        ChangeTag::Delete => "delete",
        ChangeTag::Insert => "insert",
    }
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn number(&mut self, value: u64) {
        self.bytes(&value.to_le_bytes());
    }

    fn hex(&self) -> String {
        format!("{:016x}", self.0)
    }
}

fn ops_digest(ops: &[DiffOp]) -> String {
    let mut hash = Fnv::new();
    for op in ops {
        let fields: [u64; 5] = match *op {
            DiffOp::Equal { old_index, new_index, len } => [0, old_index as u64, new_index as u64, len as u64, 0],
            DiffOp::Delete { old_index, old_len, new_index } => {
                [1, old_index as u64, old_len as u64, new_index as u64, 0]
            }
            DiffOp::Insert { old_index, new_index, new_len } => {
                [2, old_index as u64, new_index as u64, new_len as u64, 0]
            }
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                [3, old_index as u64, old_len as u64, new_index as u64, new_len as u64]
            }
        };
        for field in fields {
            hash.number(field);
        }
    }
    hash.hex()
}

fn text_diff(request: &Value) -> Value {
    let mut config = TextDiff::configure();
    config.algorithm(algorithm(request));
    if let Some(instant) = deadline(request) {
        config.deadline(instant);
    }
    if let Some(timeout) = request.get("timeout_ms").and_then(Value::as_u64) {
        config.timeout(Duration::from_millis(timeout));
    }
    if let Some(yes) = request.get("newline_terminated").and_then(Value::as_bool) {
        config.newline_terminated(yes);
    }
    let old_text = crate::text(request, "old");
    let new_text = crate::text(request, "new");
    let old_slices = strings(request, "old");
    let new_slices = strings(request, "new");
    let old_refs: Vec<&str> = old_slices.iter().map(String::as_str).collect();
    let new_refs: Vec<&str> = new_slices.iter().map(String::as_str).collect();
    let diff = match crate::text(request, "op") {
        "lines" => config.diff_lines(old_text, new_text),
        "words" => config.diff_words(old_text, new_text),
        "chars" => config.diff_chars(old_text, new_text),
        _ => config.diff_slices(&old_refs, &new_refs),
    };

    let radius = number(request, "radius", 3);
    let mut unified = diff.unified_diff();
    unified.context_radius(radius);
    if let Some(header) = request.get("header").and_then(Value::as_array) {
        unified.header(
            header.first().and_then(Value::as_str).unwrap_or_default(),
            header.get(1).and_then(Value::as_str).unwrap_or_default(),
        );
    }
    if let Some(yes) = request.get("hint").and_then(Value::as_bool) {
        unified.missing_newline_hint(yes);
    }
    let unified_text = unified.to_string();
    let mut written = Vec::new();
    unified.to_writer(&mut written).expect("write to a vector");
    assert_eq!(written, unified_text.as_bytes());

    let insertions = diff.iter_all_changes().filter(|change| change.tag() == ChangeTag::Insert).count();
    let deletions = diff.iter_all_changes().filter(|change| change.tag() == ChangeTag::Delete).count();

    if request.get("digest").and_then(Value::as_bool).unwrap_or(false) {
        let mut changes = Fnv::new();
        let mut count = 0u64;
        for change in diff.iter_all_changes() {
            count += 1;
            changes.number(match change.tag() {
                ChangeTag::Equal => 0,
                ChangeTag::Delete => 1,
                ChangeTag::Insert => 2,
            });
            changes.number(change.old_index().map_or(0, |index| index as u64 + 1));
            changes.number(change.new_index().map_or(0, |index| index as u64 + 1));
            changes.bytes(change.value().as_bytes());
            changes.number(u64::from(change.missing_newline()));
        }
        let mut text = Fnv::new();
        text.bytes(unified_text.as_bytes());
        let mut hunks = Fnv::new();
        let mut hunk_count = 0u64;
        for hunk in unified.iter_hunks() {
            hunk_count += 1;
            hunks.bytes(hunk.header().to_string().as_bytes());
            hunks.bytes(ops_digest(hunk.ops()).as_bytes());
            hunks.bytes(hunk.to_string().as_bytes());
        }
        return json!({
            "old_len": diff.old_slices().len(),
            "new_len": diff.new_slices().len(),
            "ops_len": diff.ops().len(),
            "ops": ops_digest(diff.ops()),
            "changes_len": count,
            "changes": changes.hex(),
            "insertions": insertions,
            "deletions": deletions,
            "unified_len": unified_text.len(),
            "unified": text.hex(),
            "hunks_len": hunk_count,
            "hunks": hunks.hex(),
            "ratio_bits": diff.ratio().to_bits(),
        });
    }

    let changes: Vec<Value> = diff
        .iter_all_changes()
        .map(|change| {
            json!({
                "tag": change_tag(change.tag()),
                "sign": change.tag().to_string(),
                "old_index": change.old_index(),
                "new_index": change.new_index(),
                "value": change.value(),
                "as_str": change.as_str(),
                "lossy": change.to_string_lossy(),
                "missing_newline": change.missing_newline(),
                "display": change.to_string(),
            })
        })
        .collect();
    // `TextDiff::iter_changes` op by op must be the same list.
    let by_op: Vec<Value> = diff
        .ops()
        .iter()
        .flat_map(|op| diff.iter_changes(op))
        .map(|change| json!([change_tag(change.tag()), change.old_index(), change.new_index(), change.value()]))
        .collect();
    let hunks: Vec<Value> = unified
        .iter_hunks()
        .map(|hunk| {
            let mut written = Vec::new();
            hunk.to_writer(&mut written).expect("write to a vector");
            json!({
                "header": hunk.header().to_string(),
                "ops": ops_json(hunk.ops()),
                "hint": hunk.missing_newline_hint(),
                "text": hunk.to_string(),
                "written": String::from_utf8(written).expect("strings in, strings out"),
                "changes": hunk.iter_changes().map(|change| json!([change_tag(change.tag()), change.value()])).collect::<Vec<_>>(),
            })
        })
        .collect();
    let grouped: Vec<Value> = diff.grouped_ops(radius).iter().map(|group| ops_json(group)).collect();
    json!({
        "old": diff.old_slices(),
        "new": diff.new_slices(),
        "algorithm": format!("{:?}", diff.algorithm()),
        "newline_terminated": diff.newline_terminated(),
        "ops": ops_json(diff.ops()),
        "tuples": tag_tuples_json(diff.ops()),
        "changes": changes,
        "by_op": by_op,
        "insertions": insertions,
        "deletions": deletions,
        "grouped": grouped,
        "hunks": hunks,
        "unified": unified_text,
        "ratio_bits": diff.ratio().to_bits(),
    })
}

fn tokenize(request: &Value) -> Value {
    let text = crate::text(request, "text");
    let tokens = match crate::text(request, "kind") {
        "lines" => text.tokenize_lines(),
        "lines_and_newlines" => text.tokenize_lines_and_newlines(),
        "words" => text.tokenize_words(),
        "chars" => text.tokenize_chars(),
        _ => return crate::unknown(request),
    };
    json!({
        "tokens": tokens,
        "ends_with_newline": text.ends_with_newline(),
    })
}

/// `capture_diff_slices_deadline` over numbers: the full hook stack the text
/// differ uses, without the text.
fn capture(request: &Value) -> Value {
    let old = numbers(request, "old");
    let new = numbers(request, "new");
    let ops = capture_diff_slices_deadline(algorithm(request), &old, &new, deadline(request));
    let n = number(request, "radius", 3);
    let grouped = group_diff_ops(ops.clone(), n);
    if request.get("digest").and_then(Value::as_bool).unwrap_or(false) {
        let mut groups = Fnv::new();
        for group in &grouped {
            groups.bytes(ops_digest(group).as_bytes());
        }
        return json!({
            "ops_len": ops.len(),
            "ops": ops_digest(&ops),
            "grouped_len": grouped.len(),
            "grouped": groups.hex(),
            "ratio_bits": get_diff_ratio(&ops, old.len(), new.len()).to_bits(),
        });
    }
    let changes: Vec<Value> = ops
        .iter()
        .flat_map(|op| op.iter_changes(&old, &new))
        .map(|change| json!([change_tag(change.tag()), change.old_index(), change.new_index(), change.value()]))
        .collect();
    let slices: Vec<Value> = ops
        .iter()
        .flat_map(|op| op.iter_slices(&old, &new))
        .map(|(tag, values)| json!([change_tag(tag), values]))
        .collect();
    json!({
        "ops": ops_json(&ops),
        "tuples": tag_tuples_json(&ops),
        "changes": changes,
        "slices": slices,
        "grouped": grouped.iter().map(|group| ops_json(group)).collect::<Vec<_>>(),
        "ratio_bits": get_diff_ratio(&ops, old.len(), new.len()).to_bits(),
    })
}

/// One algorithm with a chosen hook stack over sub-ranges, to compare what
/// the algorithm itself emits before the clean-up hooks rearrange it.
fn raw(request: &Value) -> Value {
    let old = numbers(request, "old");
    let new = numbers(request, "new");
    let alg = algorithm(request);
    let old_range = number(request, "old_start", 0)..number(request, "old_end", old.len());
    let new_range = number(request, "new_start", 0)..number(request, "new_end", new.len());
    let deadline = deadline(request);
    let ops = match crate::text(request, "hook") {
        "replace" => {
            let mut d = Replace::new(Capture::new());
            algorithms::diff_deadline(alg, &mut d, &old[..], old_range, &new[..], new_range, deadline).unwrap();
            d.into_inner().into_ops()
        }
        "compact" => {
            let mut d = Compact::new(Replace::new(Capture::new()), &old[..], &new[..]);
            algorithms::diff_deadline(alg, &mut d, &old[..], old_range, &new[..], new_range, deadline).unwrap();
            d.into_inner().into_inner().into_ops()
        }
        "compact_only" => {
            let mut d = Compact::new(Capture::new(), &old[..], &new[..]);
            algorithms::diff_deadline(alg, &mut d, &old[..], old_range, &new[..], new_range, deadline).unwrap();
            d.into_inner().into_ops()
        }
        _ => {
            let mut d = Capture::new();
            algorithms::diff_deadline(alg, &mut d, &old[..], old_range, &new[..], new_range, deadline).unwrap();
            d.into_ops()
        }
    };
    if request.get("digest").and_then(Value::as_bool).unwrap_or(false) {
        return json!({"ops_len": ops.len(), "ops": ops_digest(&ops)});
    }
    json!({"ops": ops_json(&ops)})
}

fn identify(request: &Value) -> Value {
    let old = strings(request, "old");
    let new = strings(request, "new");
    let old_range = number(request, "old_start", 0)..number(request, "old_end", old.len());
    let new_range = number(request, "new_start", 0)..number(request, "new_end", new.len());
    let ih = IdentifyDistinct::<u32>::new(&old[..], old_range.clone(), &new[..], new_range.clone());
    json!({
        "old": old_range.clone().map(|index| ih.old_lookup()[index]).collect::<Vec<u32>>(),
        "new": new_range.clone().map(|index| ih.new_lookup()[index]).collect::<Vec<u32>>(),
        "old_range": [ih.old_range().start, ih.old_range().end],
        "new_range": [ih.new_range().start, ih.new_range().end],
    })
}

fn op_from_json(value: &Value) -> DiffOp {
    let field = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or_default() as usize;
    match value.get("op").and_then(Value::as_str).unwrap_or_default() {
        "delete" => DiffOp::Delete {
            old_index: field("old_index"),
            old_len: field("old_len"),
            new_index: field("new_index"),
        },
        "insert" => DiffOp::Insert {
            old_index: field("old_index"),
            new_index: field("new_index"),
            new_len: field("new_len"),
        },
        "replace" => DiffOp::Replace {
            old_index: field("old_index"),
            old_len: field("old_len"),
            new_index: field("new_index"),
            new_len: field("new_len"),
        },
        _ => DiffOp::Equal { old_index: field("old_index"), new_index: field("new_index"), len: field("len") },
    }
}

/// `group_diff_ops` and the hunk header over an op list the request spells
/// out, including lists no algorithm produces. The ops are also replayed
/// through `Replace` on their own.
fn group(request: &Value) -> Value {
    let ops: Vec<DiffOp> = request
        .get("ops")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(op_from_json).collect())
        .unwrap_or_default();
    let n = number(request, "radius", 3);
    let grouped = group_diff_ops(ops.clone(), n);
    let headers: Vec<String> = grouped
        .iter()
        .filter(|group| !group.is_empty())
        .map(|group| similar::udiff::UnifiedHunkHeader::new(group).to_string())
        .collect();
    // `Replace` checks with `debug_assert_eq!` that the deletes and inserts it
    // merges follow one another. A replace op passes through it without
    // flushing them, so a script holding one can break that, and what happens
    // then depends on the build profile. Such scripts are not replayed.
    let replaced = if ops.iter().any(|op| op.tag() == DiffTag::Replace) {
        Value::Null
    } else {
        let mut replaced = Replace::new(Capture::new());
        for op in &ops {
            op.apply_to_hook(&mut replaced).unwrap();
        }
        replaced.finish().unwrap();
        ops_json(replaced.into_inner().ops())
    };
    json!({
        "grouped": grouped.iter().map(|group| ops_json(group)).collect::<Vec<_>>(),
        "headers": headers,
        "replaced": replaced,
        "ratio_bits": get_diff_ratio(&ops, number(request, "old_len", 0), number(request, "new_len", 0)).to_bits(),
    })
}

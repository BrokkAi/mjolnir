//! Byte budgets for relay events and snapshots, and the truncation that keeps
//! a payload inside one.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;

use super::{RELAY_TRUNCATION_FLOOR, RelayObservation};

pub fn ensure_serialized_budget(
    value: &impl Serialize,
    budget: usize,
    description: &str,
) -> Result<()> {
    let size = serde_json::to_vec(value)
        .with_context(|| format!("serialize {description} for size validation"))?
        .len();
    ensure_byte_budget(size, budget, description)
}

pub fn ensure_byte_budget(size: usize, budget: usize, description: &str) -> Result<()> {
    if size > budget {
        bail!("{description} is too large ({size} bytes; maximum {budget})");
    }
    Ok(())
}

/// One step in a JSON document, used to revisit a located string mutably.
#[derive(Debug, Clone, PartialEq, Eq)]
enum JsonSegment {
    Key(String),
    Index(usize),
}

/// Locate the longest string in a JSON document, with the path to reach it.
fn longest_string_path(value: &Value) -> Option<(Vec<JsonSegment>, usize)> {
    fn walk(
        value: &Value,
        path: &mut Vec<JsonSegment>,
        best: &mut Option<(Vec<JsonSegment>, usize)>,
    ) {
        match value {
            Value::String(text) => {
                if best.as_ref().is_none_or(|(_, length)| text.len() > *length) {
                    *best = Some((path.clone(), text.len()));
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    path.push(JsonSegment::Index(index));
                    walk(item, path, best);
                    path.pop();
                }
            }
            Value::Object(entries) => {
                for (key, entry) in entries {
                    path.push(JsonSegment::Key(key.clone()));
                    walk(entry, path, best);
                    path.pop();
                }
            }
            _ => {}
        }
    }

    let mut best = None;
    walk(value, &mut Vec::new(), &mut best);
    best
}

fn string_at_path<'a>(value: &'a mut Value, path: &[JsonSegment]) -> Option<&'a mut String> {
    let mut cursor = value;
    for segment in path {
        cursor = match (segment, cursor) {
            (JsonSegment::Key(key), Value::Object(entries)) => entries.get_mut(key)?,
            (JsonSegment::Index(index), Value::Array(items)) => items.get_mut(*index)?,
            _ => return None,
        };
    }
    match cursor {
        Value::String(text) => Some(text),
        _ => None,
    }
}

/// Shorten `text` to at most `keep` bytes and describe what was dropped.
/// Truncation lands on a character boundary, so the result stays valid UTF-8.
fn truncate_with_marker(text: &mut String, keep: usize) {
    let mut end = keep.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = text.len() - end;
    text.truncate(end);
    text.push_str(&format!("… [mj truncated {dropped} bytes]"));
}

/// Keep at most the last `keep` bytes of `text` and describe what was dropped.
/// The kept part starts on a character boundary, so the result stays valid
/// UTF-8. Returns whether anything was dropped.
///
/// This is the mirror of [`truncate_with_marker`] for output whose end is the
/// interesting part, such as a terminal's tail.
///
/// The Unix worker is the only production caller; the helper stays compiled
/// on Windows so its unit test still builds under `cargo test --no-run`.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn truncate_start_with_marker(text: &mut String, keep: usize) -> bool {
    if text.len() <= keep {
        return false;
    }
    let mut start = text.len() - keep;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    let dropped = start;
    text.drain(..start);
    text.insert_str(0, &format!("[mj dropped {dropped} earlier bytes]\n"));
    true
}

/// Fit an observation inside `budget` serialized bytes by shortening its
/// largest text payloads.
///
/// The ACP peer decides what the agent said; the relay only decides how much
/// of it one durable event can carry. So an oversized payload is recorded in
/// truncated form rather than rejected — refusing it would strand a live
/// session over a transport limit it cannot see or control.
pub fn clamp_observation(observation: RelayObservation, budget: usize) -> Result<RelayObservation> {
    let mut size = serde_json::to_vec(&observation)
        .context("measure relay observation")?
        .len();
    if size <= budget {
        return Ok(observation);
    }
    // Only an observation that really has to shrink pays for the JSON tree
    // the truncation pass walks.
    let mut value =
        serde_json::to_value(&observation).context("serialize relay observation for clamping")?;
    let original = size;
    while size > budget {
        let Some((path, length)) = longest_string_path(&value) else {
            break;
        };
        if length <= RELAY_TRUNCATION_FLOOR {
            break;
        }
        let Some(text) = string_at_path(&mut value, &path) else {
            break;
        };
        // Leave room for the marker itself so one pass usually suffices.
        let keep = length
            .saturating_sub(size - budget + 64)
            .max(RELAY_TRUNCATION_FLOOR);
        truncate_with_marker(text, keep);
        size = serde_json::to_vec(&value)
            .context("measure clamped relay observation")?
            .len();
    }
    if size > budget {
        return Ok(RelayObservation::Warning {
            message: format!(
                "dropped an observation that cannot be recorded: {original} bytes exceeds the {budget} byte event budget and its payload is not truncatable"
            ),
        });
    }
    match serde_json::from_value(value) {
        Ok(clamped) => {
            tracing::warn!(
                original,
                clamped = size,
                "truncated an oversized relay observation"
            );
            Ok(clamped)
        }
        Err(error) => Ok(RelayObservation::Warning {
            message: format!(
                "dropped an observation of {original} bytes: it could not be re-read after truncation: {error}"
            ),
        }),
    }
}

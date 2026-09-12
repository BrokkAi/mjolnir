//! Store an edit as a patch instead of two copies of the file.
//!
//! ACP v1 describes a file edit as `oldText` plus `newText` — the whole file
//! before and the whole file after. A three-line change to a 200 KB source
//! file therefore costs 400 KB, every time the agent touches it, forever: the
//! journal keeps the event, the projection keeps the tool call, and both are
//! read back on every resume. Measured on one real session, tool-call content
//! reached 561 MB and the largest single transcript item was 2,087,180 bytes.
//!
//! Nothing in Mjolnir ever reconstructs a file from those two copies. The only
//! reader is the diffstat — `path  +12 −3`. A unified patch answers that and
//! is proportional to the edit rather than to the file, so that is what gets
//! written. It also leaves the change renderable, which two discarded file
//! copies did not.
//!
//! The patch travels in the diff's `_meta`, which ACP reserves for exactly this
//! kind of extension, under [`DIFF_PATCH_META_KEY`]. `oldText` and `newText`
//! are cleared once it is there. Records written before this existed still
//! carry both copies, so every reader here falls back to them.

use std::time::Duration;

use agent_client_protocol::schema::v1::{Diff, ToolCallContent};
use serde::{Deserialize, Serialize};
use similar::TextDiff;

/// The `_meta` key holding a diff's patch. Namespaced because `_meta` is
/// shared with whatever the agent puts there.
pub const DIFF_PATCH_META_KEY: &str = "dev.mj.diffPatch";

/// How much patch text one edit may record.
///
/// A patch is proportional to the edit, so this only binds when an edit really
/// is enormous — a generated file replaced wholesale, say. It matches the cap
/// terminal output already gets in [`crate::relay::TERMINAL_JOURNAL_OUTPUT_BYTES`]
/// in spirit: keep the informative part, say plainly that the rest was dropped.
pub const DIFF_PATCH_BYTES: usize = 128 * 1024;

/// Lines of unchanged context kept around each hunk.
const CONTEXT_RADIUS: usize = 3;

/// How long the line diff may run before it settles for a coarser answer.
///
/// Myers is O(N·D), so a whole-file rewrite of a large generated file can take
/// arbitrarily long. This runs on the relay's recording path, where a stall is
/// a stalled session, so the diff has a deadline: past it `similar` returns a
/// valid but blunter diff instead of the minimal one.
const DIFF_DEADLINE: Duration = Duration::from_millis(250);

/// A stored edit, as a patch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffPatch {
    /// Unified diff text, headed `--- a/path` / `+++ b/path` (`/dev/null` for
    /// a created file). Same shape mjolnir's `trajectory::unified_diff` emits,
    /// which is what the diff highlighter in `src/web/tool-output.js`
    /// recognizes.
    pub text: String,
    /// Lines the patch adds.
    pub insertions: usize,
    /// Lines the patch removes.
    pub deletions: usize,
    /// Whether the file had no previous content.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub created: bool,
    /// Whether hunks were dropped to fit [`DIFF_PATCH_BYTES`]. The counts above
    /// still describe the whole edit; only the text is short.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Replace a diff's two file copies with a patch, in place.
///
/// Returns whether anything changed, so a caller can tell a rewritten update
/// from one that was already compact.
pub fn compact_diff(diff: &mut Diff) -> bool {
    if diff.old_text.is_none() && diff.new_text.is_empty() {
        return false;
    }
    let patch = build_patch(
        &diff.path.display().to_string(),
        diff.old_text.as_deref(),
        &diff.new_text,
    );
    let Ok(value) = serde_json::to_value(&patch) else {
        // Serializing a struct of strings and counts does not fail. If it
        // somehow does, keeping the original copies is the safe answer.
        return false;
    };
    diff.meta
        .get_or_insert_with(Default::default)
        .insert(DIFF_PATCH_META_KEY.to_owned(), value);
    diff.old_text = None;
    diff.new_text = String::new();
    true
}

/// Replace every file copy in a tool call's content with a patch.
pub fn compact_tool_call_content(content: &mut [ToolCallContent]) -> bool {
    content
        .iter_mut()
        .fold(false, |compacted, item| match item {
            ToolCallContent::Diff(diff) => compact_diff(diff) || compacted,
            _ => compacted,
        })
}

/// Reduce a diff to the counts a diffstat reads, dropping the patch text.
///
/// This is what a retention pass leaves behind once a checkpoint holds the
/// whole edit. A diff recorded before patches were stored is compacted first,
/// so its counts come from the file copies before they go — otherwise the
/// diffstat the transcript shows would drop to `+0 −0`.
///
/// Returns whether anything changed.
pub fn drop_patch_text(diff: &mut Diff) -> bool {
    let mut changed = compact_diff(diff);
    let Some(mut patch) = stored_patch(diff) else {
        return changed;
    };
    if patch.text.is_empty() && patch.truncated {
        return changed;
    }
    patch.text = String::new();
    patch.truncated = true;
    let Ok(value) = serde_json::to_value(&patch) else {
        return changed;
    };
    diff.meta
        .get_or_insert_with(Default::default)
        .insert(DIFF_PATCH_META_KEY.to_owned(), value);
    changed = true;
    changed
}

/// Read back the stored patch, if this diff has one.
#[must_use]
pub fn stored_patch(diff: &Diff) -> Option<DiffPatch> {
    let value = diff.meta.as_ref()?.get(DIFF_PATCH_META_KEY)?;
    match serde_json::from_value(value.clone()) {
        Ok(patch) => Some(patch),
        Err(error) => {
            tracing::warn!(%error, "could not read a stored diff patch");
            None
        }
    }
}

/// The patch for a diff, whichever form it was stored in.
///
/// A record written before diffs were compacted still holds both file copies,
/// so this diffs them on demand rather than reporting nothing.
#[must_use]
pub fn patch_of(diff: &Diff) -> DiffPatch {
    stored_patch(diff).unwrap_or_else(|| {
        build_patch(
            &diff.path.display().to_string(),
            diff.old_text.as_deref(),
            &diff.new_text,
        )
    })
}

fn build_patch(path: &str, old_text: Option<&str>, new_text: &str) -> DiffPatch {
    let created = old_text.is_none();
    let relative = path.trim_start_matches('/');
    let old_header = if created {
        "/dev/null".to_owned()
    } else {
        format!("a/{relative}")
    };
    let old_text = old_text.unwrap_or_default();
    let changes = TextDiff::configure()
        .timeout(DIFF_DEADLINE)
        .diff_lines(old_text, new_text);
    let (insertions, deletions) =
        changes
            .iter_all_changes()
            .fold((0, 0), |(insertions, deletions), change| {
                match change.tag() {
                    similar::ChangeTag::Insert => (insertions + 1, deletions),
                    similar::ChangeTag::Delete => (insertions, deletions + 1),
                    similar::ChangeTag::Equal => (insertions, deletions),
                }
            });
    let mut unified = changes.unified_diff();
    unified.context_radius(CONTEXT_RADIUS);
    // Assemble hunk by hunk so the budget is spent on whole hunks. Half a hunk
    // reads as a corrupt patch; a short patch that says so does not.
    let mut text = format!("--- {old_header}\n+++ b/{relative}\n");
    let mut truncated = false;
    for hunk in unified.iter_hunks() {
        let rendered = hunk.to_string();
        if text.len() + rendered.len() > DIFF_PATCH_BYTES {
            truncated = true;
            break;
        }
        text.push_str(&rendered);
    }
    if truncated {
        text.push_str("[mj dropped the remaining hunks]\n");
    }
    DiffPatch {
        text,
        insertions,
        deletions,
        created,
        truncated,
    }
}

#[cfg(test)]
mod tests;

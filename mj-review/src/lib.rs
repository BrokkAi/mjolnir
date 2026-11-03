//! Cumulative review of a completed coding turn by independent harness sessions.
//!
//! A *turn review* runs after the primary agent finishes a prompt: Hel captures
//! what the workspace's repositories changed since the last completed review,
//! then asks independently running reviewer sessions to inspect that same captured
//! change. The configured reviewer profile is resolved once for the group. The
//! user can inspect each session and either forward the combined findings to the
//! primary, dismiss them, or cancel the review.
//!
//! Each reviewer follows Codex's `/review` rubric and returns plain-text findings
//! or exactly `No findings.` when there are none. The driver keeps reports in
//! launch order, labels their source sessions, and declares the group clean only
//! when every reviewer completed cleanly. It owns forwarding, cancellation and
//! the durable state restored after a daemon restart.
//!
//! Terms used throughout:
//!
//! * A *reviewer* reads the user messages, changed-file counts and captured
//!   tree ids, then gets the diff through its own harness tools.
//! * A *baseline* is the Git tree id of a repository's working tree as of the
//!   last completed review. It advances only after complete review coverage;
//!   cancellation or an incomplete group leaves the changes for the next review.

pub mod delta;
pub mod driver;
pub mod lanes;
pub mod verdict;

/// How much of the primary's user messages a review prompt embeds.
pub const USER_MESSAGES_LIMIT: usize = 128 * 1024;
/// How much of the per-file line-count table a prompt embeds. At roughly 60
/// bytes a row that is several hundred files; each repository's totals come
/// first in its section, so a cut table still says how large the change is.
pub const CHANGED_FILES_LIMIT: usize = 32 * 1024;
/// How much of each reviewer's findings is retained before aggregation.
pub const SYNTHESIS_LIMIT: usize = 32 * 1024;
/// Bound the diff retained in a capture for existing review status consumers.
pub const REVIEW_CAPTURE_DIFF_LIMIT: usize = 96 * 1024;
/// Bound a captured Git patch retained for existing review status consumers.
#[must_use]
pub fn bound_review_section(text: &str, limit: usize, label: &str) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let marker = format!("\n…[{label} omitted]…\n");
    let available = limit.saturating_sub(marker.len());
    let head = available.saturating_mul(3) / 4;
    let tail = available.saturating_sub(head);
    let head_end = text.floor_char_boundary(head);
    let tail_start = text.ceil_char_boundary(text.len().saturating_sub(tail));
    format!("{}{}{}", &text[..head_end], marker, &text[tail_start..])
}

/// Model-authored prose puts its conclusions first, so bound it by keeping the
/// head rather than both ends.
#[must_use]
pub fn bound_tail(text: &str, limit: usize, label: &str) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let marker = format!("\n…[{label} truncated]…");
    let head = text.floor_char_boundary(limit.saturating_sub(marker.len()));
    format!("{}{}", &text[..head], marker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounding_a_section_keeps_both_ends_and_marks_the_gap() {
        let text = "a".repeat(400) + &"z".repeat(400);
        let bounded = bound_review_section(&text, 200, "workspace diff");
        assert!(bounded.len() <= 200);
        assert!(bounded.starts_with("aaa"));
        assert!(bounded.ends_with("zzz"));
        assert!(bounded.contains("…[workspace diff omitted]…"));
    }

    #[test]
    fn bounding_prose_keeps_the_head_where_conclusions_are() {
        let text = format!("{}{}", "head".repeat(100), "tail".repeat(100));
        let bounded = bound_tail(&text, 120, "synthesis");
        assert!(bounded.starts_with("headhead"));
        assert!(bounded.ends_with("…[synthesis truncated]…"));
        assert!(!bounded.contains("tail"));
    }
}

//! Cumulative review of a completed coding turn by a second harness.
//!
//! A *turn review* runs after the primary agent finishes a prompt: Hel captures
//! what the workspace's repositories changed since the last completed review,
//! then asks a reviewer harness -- usually a different one from the primary's --
//! to look for defects in exactly that change. The user watches it happen in the
//! split pane and either forwards the findings to the primary, dismisses them,
//! or cancels.
//!
//! The review engine is ported from the sibling `mjolnir` repository's
//! `mj-agents/src/discrete_review.rs`, whose prompts, lane roster and verdict
//! contract this module keeps as close to the original as the different runtime
//! allows: mj runs review agents through its own subagent pool, while Hel runs
//! them as reviewer sidecars inside the session's worker container. Keeping the
//! text identical is deliberate -- the two repositories may re-merge, so every
//! gratuitous divergence is a cost. `.agents/docs/turn-review-mj-parity.md`
//! records exactly what was ported and what was not.
//!
//! Terms used throughout:
//!
//! * A *lane* is one read-only specialist reviewer with a narrow brief (control
//!   flow, duplication, error handling, dead code, tests, contracts).
//! * The *quick tier* is one general reviewer plus, only when it reports
//!   something, a validator that verifies each finding against source.
//! * The *extended tier* adds a supervisor that chooses which lanes to launch
//!   and synthesizes their reports into one verdict.
//! * A *baseline* is the Git tree id of a repository's working tree as of the
//!   last completed review. It advances only when a review resolves, so
//!   cancelling one review folds its changes into the next.

pub mod bifrost;
pub mod delta;
pub mod driver;
pub mod lanes;
pub mod mcp;
pub mod verdict;

/// How much of a lane report the next prompt may quote.
pub const LANE_REPORT_LIMIT: usize = 16 * 1024;
/// How much of the intent analyst's brief the supervisor prompt embeds.
pub const INTENT_BRIEF_LIMIT: usize = 16 * 1024;
/// How much of the primary's user messages a review prompt embeds.
pub const USER_MESSAGES_LIMIT: usize = 128 * 1024;
/// How much of Bifrost's changed-callable packet a prompt embeds.
pub const CHANGED_FUNCTIONS_LIMIT: usize = 32 * 1024;
/// How much of a synthesis is retained as the review's verdict text.
pub const SYNTHESIS_LIMIT: usize = 32 * 1024;
/// How much captured diff any one reviewing role sees. Six copies of an
/// unbounded diff is the one place this design can blow up a context window.
pub const LANE_DIFF_LIMIT: usize = 96 * 1024;

/// Bound a section of tagged evidence, keeping the head and the tail so a
/// truncated diff still shows where the change ends.
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

/// Model-authored prose (a lane report, a synthesis) puts its conclusions
/// first, so bound it by keeping the head rather than both ends.
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

    #[test]
    fn bounding_leaves_short_text_untouched() {
        assert_eq!(bound_review_section("short", 100, "diff"), "short");
        assert_eq!(bound_tail("short", 100, "synthesis"), "short");
    }
}

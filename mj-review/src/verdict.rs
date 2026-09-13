//! Interpret reviewer replies conservatively.

use super::{SYNTHESIS_LIMIT, bound_tail};
pub use mj_core::review::verdict::*;

/// Classify a supervisor's or validator's reply.
///
/// Some models explain their clean verdict before emitting the required
/// sentinel, so a final sentinel line counts as clean unless the reply also
/// contains a canonical priority marker. A priority marker records a review
/// issue and must therefore produce a findings verdict, whether it is P0 or P3.
#[must_use]
pub fn synthesis_verdict(text: &str) -> ReviewVerdict {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return ReviewVerdict::Failed {
            reason: "the review supervisor returned an empty synthesis".to_string(),
        };
    }
    let lines = trimmed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let has_priority_marker = has_priority_marker(&lines);
    let ends_with_clean_sentinel = lines.last().is_some_and(|line| {
        line.trim_matches('*')
            .trim()
            .eq_ignore_ascii_case(CLEAN_SENTINEL)
    });
    if ends_with_clean_sentinel && !has_priority_marker {
        return ReviewVerdict::Clean;
    }
    let synthesis = bound_tail(trimmed, SYNTHESIS_LIMIT, "synthesis");
    ReviewVerdict::Findings {
        synthesis,
        evidence: ReviewPassEvidence::default(),
    }
}

/// Whether a lane -- or the quick tier's sole reviewer -- reported nothing
/// worth validating. Conservative in the same direction as
/// [`synthesis_verdict`]: a reply carrying any priority marker, or one that
/// does not end in the clean sentinel, still costs a validation pass rather
/// than releasing the turn unchecked.
#[must_use]
pub fn lane_report_is_clean(text: &str) -> bool {
    let lines = text
        .trim()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let ends_clean = lines.last().is_some_and(|line| {
        line.trim_matches('*')
            .trim()
            .eq_ignore_ascii_case(LANE_CLEAN_SENTINEL)
    });
    ends_clean && !has_priority_marker(&lines)
}

fn has_priority_marker(lines: &[&str]) -> bool {
    lines.iter().any(|line| {
        let lower = line.to_ascii_lowercase();
        ["[p0]", "[p1]", "[p2]", "[p3]"]
            .iter()
            .any(|marker| lower.contains(marker))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_reviewer_report_is_clean_only_without_findings() {
        assert!(lane_report_is_clean(LANE_CLEAN_SENTINEL));
        assert!(lane_report_is_clean("\n  no FINDINGS.  \n"));
        assert!(lane_report_is_clean("**No findings.**"));
        assert!(!lane_report_is_clean("   \n "));
        assert!(!lane_report_is_clean(
            "[P2] src/a.rs:1 -- stale comment (evidence: source-reviewed)"
        ));
        // A sentinel that trails real findings is contradictory output; keep
        // the conservative direction and spend the validation pass.
        assert!(!lane_report_is_clean(
            "[P0] src/a.rs:1 -- swallowed error (evidence: source-reviewed)\nNo findings."
        ));
        // Prose without a sentinel is not a clean result either.
        assert!(!lane_report_is_clean(
            "I reviewed the diff and everything looked reasonable to me."
        ));
    }

    #[test]
    fn synthesis_verdict_classification() {
        assert!(matches!(
            synthesis_verdict("   \n  "),
            ReviewVerdict::Failed { .. }
        ));
        assert_eq!(synthesis_verdict(CLEAN_SENTINEL), ReviewVerdict::Clean);
        assert_eq!(
            synthesis_verdict("\n\n  no MATERIAL findings.   \n"),
            ReviewVerdict::Clean
        );
        assert_eq!(
            synthesis_verdict(
                "I inspected the changed paths and vetted the reviewer reports. Nothing actionable survived.\n\nNo material findings."
            ),
            ReviewVerdict::Clean,
            "harmless rationale before the final clean sentinel must not trigger correction"
        );
        assert!(matches!(
            synthesis_verdict("[P1] src/a.rs:1 -- broken\n\nNo material findings."),
            ReviewVerdict::Findings { .. }
        ));
        assert!(matches!(
            synthesis_verdict("No material findings.\n\nAdditional rationale after the verdict."),
            ReviewVerdict::Findings { .. }
        ));
        assert_eq!(
            synthesis_verdict("Inspected the changed paths.\n\n**No material findings.**"),
            ReviewVerdict::Clean,
            "Markdown emphasis around the final sentinel must not trigger correction"
        );
        assert!(matches!(
            synthesis_verdict(
                "Review summary:\n- [P2] src/a.rs:2 -- still broken\n\nNo material findings."
            ),
            ReviewVerdict::Findings { .. }
        ));
        assert!(matches!(
            synthesis_verdict("[P3] src/a.rs:1 -- optional cleanup"),
            ReviewVerdict::Findings { .. }
        ));
        assert!(matches!(
            synthesis_verdict("[P2] src/a.rs:1 -- minor\n[P1] src/b.rs:2 -- broken"),
            ReviewVerdict::Findings { .. }
        ));

        let oversize = format!("[P0] src/a.rs:1 -- {}", "x".repeat(SYNTHESIS_LIMIT * 2));
        let ReviewVerdict::Findings { synthesis, .. } = synthesis_verdict(&oversize) else {
            panic!("oversize findings must classify as findings");
        };
        assert!(synthesis.len() <= SYNTHESIS_LIMIT);
        assert!(synthesis.starts_with("[P0] src/a.rs:1"));
        assert!(synthesis.contains("[synthesis truncated]"));
    }

    #[test]
    fn a_lane_outcome_describes_itself_for_the_coverage_packet() {
        assert_eq!(LaneOutcome::Completed.describe(), "completed");
        assert_eq!(LaneOutcome::Cancelled.describe(), "cancelled");
        assert_eq!(
            LaneOutcome::Failed {
                reason: "harness exited".to_string()
            }
            .describe(),
            "failed: harness exited"
        );
    }
}

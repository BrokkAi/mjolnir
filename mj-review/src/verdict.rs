//! Classify plain-text reviewer output.

use super::{SYNTHESIS_LIMIT, bound_tail};
pub use mj_core::review::verdict::*;

/// Exact trimmed reply that signals a clean review.
pub const NO_FINDINGS: &str = "No findings.";

/// Classify a reviewer reply. Only the explicit sentinel is clean; an empty
/// answer fails closed, and all other text is retained as findings.
#[must_use]
pub fn review_output_verdict(text: &str) -> ReviewVerdict {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return ReviewVerdict::Failed {
            reason: "the reviewer returned an empty report".to_string(),
        };
    }
    if trimmed.eq_ignore_ascii_case(NO_FINDINGS) {
        return ReviewVerdict::Clean;
    }
    ReviewVerdict::Findings {
        synthesis: bound_tail(trimmed, SYNTHESIS_LIMIT, "review output"),
        evidence: ReviewPassEvidence::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_trimmed_sentinel_is_clean_case_insensitively() {
        for text in ["No findings.", "  no FINDINGS. \n"] {
            assert_eq!(review_output_verdict(text), ReviewVerdict::Clean);
        }
    }

    // Hard-won: b535fb8d2: an empty JSON object was mistaken for a clean verdict.
    #[test]
    fn prose_and_json_shaped_text_are_findings() {
        for raw in [
            "There is a race in retry handling.",
            "No findings. One nit: this branch has a bug.",
            "{}",
        ] {
            assert!(matches!(
                review_output_verdict(raw),
                ReviewVerdict::Findings { synthesis, .. } if synthesis == raw
            ));
        }
    }

    #[test]
    fn empty_text_fails_and_large_findings_are_bounded() {
        assert!(matches!(
            review_output_verdict(" \n\t"),
            ReviewVerdict::Failed { .. }
        ));
        let raw = "x".repeat(70 * 1024);
        assert!(matches!(
            review_output_verdict(&raw),
            ReviewVerdict::Findings { synthesis, .. } if synthesis.len() <= SYNTHESIS_LIMIT && synthesis.contains("truncated")
        ));
    }
}

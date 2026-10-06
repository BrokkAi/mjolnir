//! Parse, classify and render the Codex review JSON output.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{SYNTHESIS_LIMIT, bound_tail};
pub use mj_core::review::verdict::*;

/// The JSON result returned by Codex's `/review` prompt.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewOutput {
    pub findings: Vec<ReviewFinding>,
    pub overall_correctness: String,
    pub overall_explanation: String,
    pub overall_confidence_score: f32,
}

/// One structured finding from a review.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewFinding {
    pub title: String,
    pub body: String,
    pub confidence_score: f32,
    /// The rubric allows `null` or an omitted priority.
    pub priority: Option<i32>,
    pub code_location: CodeLocation,
}

/// Location associated with a finding.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeLocation {
    pub absolute_file_path: PathBuf,
    pub line_range: LineRange,
}

/// Inclusive line range associated with a finding.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

/// Parsed output plus whether it came from structured JSON rather than the
/// conservative raw-text fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedReviewOutput {
    pub output: ReviewOutput,
    pub structured: bool,
}

/// Parse a ReviewOutput from a text blob, accepting JSON wrapped in prose or
/// fences just as Codex's `parse_review_output_event` does.
#[must_use]
pub fn parse_review_output_event(text: &str) -> ParsedReviewOutput {
    if let Ok(output) = serde_json::from_str::<ReviewOutput>(text) {
        return ParsedReviewOutput {
            output,
            structured: true,
        };
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
        && start < end
        && let Some(slice) = text.get(start..=end)
        && let Ok(output) = serde_json::from_str::<ReviewOutput>(slice)
    {
        return ParsedReviewOutput {
            output,
            structured: true,
        };
    }
    ParsedReviewOutput {
        output: ReviewOutput {
            overall_explanation: text.to_string(),
            ..ReviewOutput::default()
        },
        structured: false,
    }
}

/// Classify a nonblank reviewer reply. A structured empty findings array is
/// clean; unstructured text remains a finding so malformed output is visible.
#[must_use]
pub fn review_output_verdict(text: &str) -> ReviewVerdict {
    if text.trim().is_empty() {
        return ReviewVerdict::Failed {
            reason: "the reviewer returned an empty report".to_string(),
        };
    }
    let parsed = parse_review_output_event(text);
    if parsed.structured && parsed.output.findings.is_empty() {
        return ReviewVerdict::Clean;
    }
    let rendered = render_review_output_text(&parsed.output);
    ReviewVerdict::Findings {
        synthesis: bound_tail(&rendered, SYNTHESIS_LIMIT, "review output"),
        evidence: ReviewPassEvidence::default(),
    }
}

fn format_location(finding: &ReviewFinding) -> String {
    let path = finding.code_location.absolute_file_path.display();
    let start = finding.code_location.line_range.start;
    let end = finding.code_location.line_range.end;
    format!("{path}:{start}-{end}")
}

/// Render Codex's plain-text findings block.
#[must_use]
pub fn format_review_findings_block(findings: &[ReviewFinding]) -> String {
    let mut lines = vec![String::new()];
    lines.push(if findings.len() > 1 {
        "Full review comments:".to_string()
    } else {
        "Review comment:".to_string()
    });
    for finding in findings {
        lines.push(String::new());
        lines.push(format!(
            "- {} — {}",
            finding.title,
            format_location(finding)
        ));
        for body_line in finding.body.lines() {
            lines.push(format!("  {body_line}"));
        }
    }
    lines.join("\n")
}

/// Render an explanation and the findings in Codex's review format.
#[must_use]
pub fn render_review_output_text(output: &ReviewOutput) -> String {
    let mut sections = Vec::new();
    let explanation = output.overall_explanation.trim();
    if !explanation.is_empty() {
        sections.push(explanation.to_string());
    }
    if !output.findings.is_empty() {
        let findings = format_review_findings_block(&output.findings);
        let trimmed = findings.trim();
        if !trimmed.is_empty() {
            sections.push(trimmed.to_string());
        }
    }
    if sections.is_empty() {
        "Reviewer failed to output a response.".to_string()
    } else {
        sections.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FINDING_JSON: &str = r#"{
        "findings":[{
            "title":"[P1] Preserve the request",
            "body":"A retry can lose the accepted request.",
            "confidence_score":0.96,
            "priority":1,
            "code_location":{
                "absolute_file_path":"/workspace/src/lib.rs",
                "line_range":{"start":4,"end":7}
            }
        }],
        "overall_correctness":"patch is incorrect",
        "overall_explanation":"The retry path drops the request."
    }"#;

    #[test]
    fn parses_pure_json_and_defaults_omitted_fields() {
        let parsed = parse_review_output_event(r#"{"findings":[],"overall_explanation":"clean"}"#);
        assert!(parsed.structured);
        assert!(parsed.output.findings.is_empty());
        assert_eq!(parsed.output.overall_explanation, "clean");
        assert_eq!(parsed.output.overall_confidence_score, 0.0);
        let partial = parse_review_output_event(r#"{"findings":[{"title":"partial"}]}"#);
        assert!(partial.structured);
        assert_eq!(partial.output.findings[0].body, "");
        assert_eq!(partial.output.findings[0].code_location.line_range.start, 0);
        let null_priority =
            parse_review_output_event(r#"{"findings":[{"title":"t","priority":null}]}"#);
        assert!(null_priority.structured);
        assert_eq!(null_priority.output.findings[0].priority, None);
    }

    #[test]
    fn parses_json_wrapped_in_prose_or_fences() {
        for text in [
            format!("Review complete:\n{FINDING_JSON}"),
            format!("```json\n{FINDING_JSON}\n```"),
        ] {
            let parsed = parse_review_output_event(&text);
            assert!(parsed.structured, "{text}");
            assert_eq!(parsed.output.findings.len(), 1);
        }
    }

    #[test]
    fn a_structured_empty_findings_array_is_clean() {
        assert_eq!(
            review_output_verdict(r#"{"findings":[],"overall_explanation":"Looks good."}"#),
            ReviewVerdict::Clean
        );
    }

    #[test]
    fn unparseable_text_is_preserved_as_findings() {
        let raw = "I could not produce JSON, but this change looks risky.";
        assert!(matches!(
            review_output_verdict(raw),
            ReviewVerdict::Findings { synthesis, .. } if synthesis == raw
        ));
    }

    #[test]
    fn blank_text_fails_the_review() {
        assert!(matches!(
            review_output_verdict(" \n\t"),
            ReviewVerdict::Failed { .. }
        ));
    }

    #[test]
    fn renders_a_review_output_snapshot() {
        let parsed = parse_review_output_event(FINDING_JSON);
        assert_eq!(
            render_review_output_text(&parsed.output),
            "The retry path drops the request.\n\nReview comment:\n\n- [P1] Preserve the request — /workspace/src/lib.rs:4-7\n  A retry can lose the accepted request."
        );
    }
}

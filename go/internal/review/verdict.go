// Port of mj-review/src/verdict.rs.
//
// Classify plain-text reviewer output. The verdict types themselves are
// core's (Rust re-exports mj_core::review::verdict::*).

package review

import (
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// NoFindings is the exact trimmed reply that signals a clean review.
const NoFindings = "No findings."

// ReviewOutputVerdict classifies a reviewer reply. Only the explicit sentinel
// is clean; an empty answer fails closed, and all other text is retained as
// findings.
func ReviewOutputVerdict(text string) core.ReviewVerdict {
	trimmed := strings.TrimSpace(text)
	if trimmed == "" {
		return core.ReviewVerdictFailed{Reason: "the reviewer returned an empty report"}
	}
	if equalIgnoreASCIICase(trimmed, NoFindings) {
		return core.ReviewVerdictClean{}
	}
	return core.ReviewVerdictFindings{
		Synthesis: BoundTail(trimmed, SynthesisLimit, "review output"),
		Evidence:  core.ReviewPassEvidence{},
	}
}

// equalIgnoreASCIICase is Rust's str::eq_ignore_ascii_case: only ASCII
// letters fold, unlike strings.EqualFold.
func equalIgnoreASCIICase(a, b string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := 0; i < len(a); i++ {
		x, y := a[i], b[i]
		if 'A' <= x && x <= 'Z' {
			x += 'a' - 'A'
		}
		if 'A' <= y && y <= 'Z' {
			y += 'a' - 'A'
		}
		if x != y {
			return false
		}
	}
	return true
}

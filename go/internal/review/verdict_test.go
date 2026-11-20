// Port of the tests in mj-review/src/verdict.rs.

package review

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestExactTrimmedSentinelIsCleanCaseInsensitively(t *testing.T) {
	for _, text := range []string{"No findings.", "  no FINDINGS. \n"} {
		if verdict := ReviewOutputVerdict(text); verdict != (core.ReviewVerdictClean{}) {
			t.Fatalf("%q: %#v", text, verdict)
		}
	}
}

// Hard-won: b535fb8d2: an empty JSON object was mistaken for a clean verdict.
func TestProseAndJSONShapedTextAreFindings(t *testing.T) {
	for _, raw := range []string{
		"There is a race in retry handling.",
		"No findings. One nit: this branch has a bug.",
		"{}",
	} {
		findings, ok := ReviewOutputVerdict(raw).(core.ReviewVerdictFindings)
		if !ok || findings.Synthesis != raw {
			t.Fatalf("%q: %#v", raw, ReviewOutputVerdict(raw))
		}
	}
}

func TestEmptyTextFailsAndLargeFindingsAreBounded(t *testing.T) {
	if _, ok := ReviewOutputVerdict(" \n\t").(core.ReviewVerdictFailed); !ok {
		t.Fatalf("%#v", ReviewOutputVerdict(" \n\t"))
	}
	raw := strings.Repeat("x", 70*1024)
	findings, ok := ReviewOutputVerdict(raw).(core.ReviewVerdictFindings)
	if !ok || len(findings.Synthesis) > SynthesisLimit || !strings.Contains(findings.Synthesis, "truncated") {
		t.Fatalf("large findings were not bounded: ok=%v, %d bytes", ok, len(findings.Synthesis))
	}
}

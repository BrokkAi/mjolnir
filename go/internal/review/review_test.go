// Port of the tests in mj-review/src/lib.rs.

package review

import (
	"strings"
	"testing"
)

func TestBoundingASectionKeepsBothEndsAndMarksTheGap(t *testing.T) {
	text := strings.Repeat("a", 400) + strings.Repeat("z", 400)
	bounded := BoundReviewSection(text, 200, "workspace diff")
	if len(bounded) > 200 {
		t.Fatalf("bounded to %d bytes", len(bounded))
	}
	if !strings.HasPrefix(bounded, "aaa") {
		t.Fatalf("%q", bounded)
	}
	if !strings.HasSuffix(bounded, "zzz") {
		t.Fatalf("%q", bounded)
	}
	if !strings.Contains(bounded, "…[workspace diff omitted]…") {
		t.Fatalf("%q", bounded)
	}
}

func TestBoundingProseKeepsTheHeadWhereConclusionsAre(t *testing.T) {
	text := strings.Repeat("head", 100) + strings.Repeat("tail", 100)
	bounded := BoundTail(text, 120, "synthesis")
	if !strings.HasPrefix(bounded, "headhead") {
		t.Fatalf("%q", bounded)
	}
	if !strings.HasSuffix(bounded, "…[synthesis truncated]…") {
		t.Fatalf("%q", bounded)
	}
	if strings.Contains(bounded, "tail") {
		t.Fatalf("%q", bounded)
	}
}

// Port of linkify 0.11.0 tests/common/mod.rs

package linkify_test

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/linkify"
)

func itCommonAssertLinkedWith(t *testing.T, finder *linkify.LinkFinder, input string, expected string) {
	t.Helper()
	actual := itCommonShowLinks(input, finder)
	itCommonAssertEq(t, actual, expected)
}

func itCommonShowLinks(input string, finder *linkify.LinkFinder) string {
	var result strings.Builder

	for span := range finder.Spans(input).All() {
		if span.Kind().IsSome() {
			result.WriteByte('|')
			result.WriteString(span.AsStr())
			result.WriteByte('|')
		} else {
			result.WriteString(span.AsStr())
		}
	}
	return result.String()
}

// itCommonAssertEq is `assert_eq!` for values Go compares with ==.
func itCommonAssertEq[T comparable](t *testing.T, left T, right T) {
	t.Helper()
	if left != right {
		t.Fatalf("assertion `left == right` failed\n  left: %#v\n right: %#v", left, right)
	}
}

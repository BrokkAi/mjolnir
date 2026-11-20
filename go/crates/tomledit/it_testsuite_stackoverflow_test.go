// Port of toml_edit 0.20.2 tests/testsuite/stackoverflow.rs
//
// The tests are `#[cfg(not(feature = "unbounded"))]`; the port, like
// Mjolnir's build, has no `unbounded` feature.

package tomledit_test

import (
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/tomledit"
)

// recursionDepths are the `(depth, is_ok)` pairs each test checks.
var recursionDepths = []struct {
	depth int
	isOk  bool
}{{1, true}, {20, true}, {300, false}}

func checkRecursion(t *testing.T, input func(depth int) string) {
	t.Helper()
	for _, c := range recursionDepths {
		_, err := tomledit.DocumentFromStr(input(c.depth))
		if (err == nil) != c.isOk {
			t.Fatalf("assertion `left == right` failed: depth: %s\n  left: %t\n right: %t", strconv.Itoa(c.depth), err == nil, c.isOk)
		}
	}
}

func TestItTestsuiteStackoverflow_array_recursion_limit(t *testing.T) {
	checkRecursion(t, func(depth int) string {
		return "x=" + strings.Repeat("[", depth) + strings.Repeat("]", depth)
	})
}

func TestItTestsuiteStackoverflow_inline_table_recursion_limit(t *testing.T) {
	checkRecursion(t, func(depth int) string {
		return "x=" + strings.Repeat("{ x = ", depth) + "true" + strings.Repeat("}", depth)
	})
}

func TestItTestsuiteStackoverflow_table_key_recursion_limit(t *testing.T) {
	checkRecursion(t, func(depth int) string {
		return "[x" + strings.Repeat(".x", depth) + "]"
	})
}

func TestItTestsuiteStackoverflow_dotted_key_recursion_limit(t *testing.T) {
	checkRecursion(t, func(depth int) string {
		return "x" + strings.Repeat(".x", depth) + " = true"
	})
}

func TestItTestsuiteStackoverflow_inline_dotted_key_recursion_limit(t *testing.T) {
	checkRecursion(t, func(depth int) string {
		return "x = { x" + strings.Repeat(".x", depth) + " = true }"
	})
}

package tomledit_test

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/tomledit"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Helpers the ports of toml_edit's integration tests (tests/*.rs) share.
// They stand for the assertion macros and snapbox, which have no Rust file
// of their own here.

// itAssertEq is `assert_eq!(left, right)`: structural equality, and Rust's
// message with both sides in Debug form.
func itAssertEq[T any](t *testing.T, left T, right T) {
	t.Helper()
	if !rs.Eq(left, right) {
		t.Fatalf("assertion `left == right` failed\n  left: %s\n right: %s", rs.DebugString(left), rs.DebugString(right))
	}
}

// itAssert is `assert!(cond)`.
func itAssert(t *testing.T, cond bool, what string) {
	t.Helper()
	if !cond {
		t.Fatalf("assertion failed: %s", what)
	}
}

// snapboxNormalize is the newline normalization snapbox's `assert_eq`
// applies to both sides (normalize-line-endings): `\r\n` and a lone `\r`
// both become `\n`.
func snapboxNormalize(text string) string {
	text = strings.ReplaceAll(text, "\r\n", "\n")
	return strings.ReplaceAll(text, "\r", "\n")
}

// snapboxAssertEq is snapbox::assert_eq(expected, actual).
func snapboxAssertEq(t *testing.T, expected string, actual string) {
	t.Helper()
	if snapboxNormalize(expected) != snapboxNormalize(actual) {
		t.Fatalf("snapbox: expected\n%s\n---- actual\n%s", expected, actual)
	}
}

// itParseDocument is `s.parse::<Document>().unwrap()`.
func itParseDocument(t *testing.T, s string) tomledit.Document {
	t.Helper()
	document, err := tomledit.DocumentFromStr(s)
	if err != nil {
		t.Fatalf("called `Result::unwrap()` on an `Err` value: %v", err)
	}
	return document
}

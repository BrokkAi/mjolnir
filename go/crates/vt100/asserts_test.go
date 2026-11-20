package vt100_test

import (
	"os"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The assertion macros of Rust's standard library, for the ports of the
// crate's tests (it_*_test.go). They compare as derive(PartialEq) does
// (rs.Eq, which uses Cell's own PartialEq) and print as Debug does, and they
// stop the test as a Rust panic would.

// assertEq is `assert_eq!(left, right)`.
func assertEq[T any](t testing.TB, left, right T) {
	t.Helper()
	if !rs.Eq(left, right) {
		t.Fatalf("assertion `left == right` failed\n  left: %s\n right: %s",
			rs.Format("{:?}", left), rs.Format("{:?}", right))
	}
}

// assertTrue is `assert!(condition)`; expression is the condition's text.
func assertTrue(t testing.TB, condition bool, expression string) {
	t.Helper()
	if !condition {
		t.Fatalf("assertion failed: %s", expression)
	}
}

// pair is a `(u16, u16)`, which the screen returns as two results.
func pair(a, b uint16) rs.Tuple2[uint16, uint16] {
	return rs.T2(a, b)
}

// ignored is `#[ignore]`: the test runs only when MJ_RUN_IGNORED=1, as an
// ignored Rust test runs only with --ignored.
func ignored(t testing.TB) {
	t.Helper()
	if os.Getenv("MJ_RUN_IGNORED") != "1" {
		t.Skip("#[ignore] in Rust; set MJ_RUN_IGNORED=1 to run it")
	}
}

// nth is `iterator.nth(n)` over one of the screen's row iterators.
func nth[T any](rows func(yield func(T) bool), n int) rs.Option[T] {
	i := 0
	for row := range rows {
		if i == n {
			return rs.Some(row)
		}
		i++
	}
	return rs.None[T]()
}

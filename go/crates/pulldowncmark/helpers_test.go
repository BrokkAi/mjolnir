package pulldowncmark

import (
	"reflect"
	"testing"
)

// The ported tests assert with these helpers. They stand in for
// go/rs/rstest's AssertEq and Assert, which did not exist when this package
// was written; like Rust's assert macros they stop the test at the first
// failure.

// assertEq is `assert_eq!(left, right)`.
func assertEq[T any](t *testing.T, left T, right T) {
	t.Helper()
	if !reflect.DeepEqual(left, right) {
		t.Fatalf("assertion `left == right` failed\n  left: %#v\n right: %#v", left, right)
	}
}

// assert is `assert!(cond)`.
func assert(t *testing.T, cond bool) {
	t.Helper()
	if !cond {
		t.Fatalf("assertion failed")
	}
}

package rssync

import (
	"os"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The helpers every test file of this package shares. Test files declare
// package-level names only with their own prefix, so that the files do not
// collide.

// assertEq is assert_eq! for comparable values.
func assertEq[T comparable](t testing.TB, got, want T, message ...any) {
	t.Helper()
	if got != want {
		t.Fatalf("assertion `left == right` failed %v\n  left: %#v\n right: %#v", message, got, want)
	}
}

// assertNe is assert_ne! for comparable values.
func assertNe[T comparable](t testing.TB, got, other T, message ...any) {
	t.Helper()
	if got == other {
		t.Fatalf("assertion `left != right` failed %v\n  left: %#v\n right: %#v", message, got, other)
	}
}

// assert is assert!.
func assert(t testing.TB, condition bool, message ...any) {
	t.Helper()
	if !condition {
		t.Fatalf("assertion failed: %v", message)
	}
}

// panicMessage runs f and returns the message it panics with, or fails the
// test when it does not panic.
func panicMessage(t testing.TB, f func()) (message string) {
	t.Helper()
	defer func() {
		recovered := recover()
		if recovered == nil {
			t.Fatalf("the function did not panic")
		}
		message = rs.PanicInfoOf(recovered).Message
	}()
	f()
	return ""
}

// shouldPanic is #[should_panic(expected = "...")]: f must panic with a
// message that contains expected.
func shouldPanic(t testing.TB, expected string, f func()) {
	t.Helper()
	message := panicMessage(t, f)
	if !strings.Contains(message, expected) {
		t.Fatalf("panic message %q does not contain %q", message, expected)
	}
}

// quietPanics silences the panic hook for the rest of the test, for tests
// whose threads panic on purpose. Rust's test harness prints those panics;
// the port's tests would only add noise.
func quietPanics(t testing.TB) {
	t.Helper()
	rs.SetPanicHook(func(*rs.PanicInfo) {})
	t.Cleanup(func() { rs.TakePanicHook() })
}

// formatterReady reports whether rs.Format is implemented yet. Until it is,
// the tests of FmtDisplay and FmtDebug are skipped.
func formatterReady() (ready bool) {
	defer func() {
		if recover() != nil {
			ready = false
		}
	}()
	return rs.Format("{:>3}", "a") == "  a"
}

// recording reports whether this run rewrites the oracle corpora.
func recording() bool { return os.Getenv("MJ_ORACLE") == "record" }

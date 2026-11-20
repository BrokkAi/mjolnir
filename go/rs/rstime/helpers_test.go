package rstime

import (
	"os"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// assertEq is assert_eq! for comparable values.
func assertEq[T comparable](t *testing.T, got, want T) {
	t.Helper()
	if got != want {
		t.Fatalf("assertion `left == right` failed\n  left: %v\n right: %v", got, want)
	}
}

// assertNe is assert_ne! for comparable values.
func assertNe[T comparable](t *testing.T, got, other T) {
	t.Helper()
	if got == other {
		t.Fatalf("assertion `left != right` failed\n  left: %v\n right: %v", got, other)
	}
}

// assert is assert!.
func assert(t *testing.T, condition bool, message ...any) {
	t.Helper()
	if !condition {
		t.Fatalf("assertion failed: %v", message)
	}
}

// panicMessage runs f and returns the message it panics with, or fails the
// test when it does not panic.
func panicMessage(t *testing.T, f func()) (message string) {
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
func shouldPanic(t *testing.T, expected string, f func()) {
	t.Helper()
	message := panicMessage(t, f)
	if !strings.Contains(message, expected) {
		t.Fatalf("panic message %q does not contain %q", message, expected)
	}
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

// some and none shorten Option values of durations in assertions.
func some(d time.Duration) rs.Option[time.Duration] { return rs.Some(d) }

func none() rs.Option[time.Duration] { return rs.None[time.Duration]() }

// debug is `format!("{:?}", Duration::new(secs, nanos))`.
func debug(secs uint64, nanos uint32) string {
	return rs.Format("{:?}", DurationNew(secs, nanos))
}

// debugPrecision is `format!("{:.N?}", Duration::new(secs, nanos))`.
func debugPrecision(precision int, secs uint64, nanos uint32) string {
	return rs.Format("{:.p$?}", DurationNew(secs, nanos), rs.Arg("p", precision))
}

// debugPadded is `format!("{:<W.P?}", ..)` and its relatives: precision is
// negative for none.
func debugPadded(align rs.Option[rs.Alignment], width int, precision int, secs uint64, nanos uint32) string {
	spec := "{:"
	if alignment, ok := align.Get(); ok {
		spec += map[rs.Alignment]string{rs.Alignment_Left: "<", rs.Alignment_Right: ">", rs.Alignment_Center: "^"}[alignment]
	}
	spec += "w$"
	args := []any{DurationNew(secs, nanos), rs.Arg("w", width)}
	if precision >= 0 {
		spec += ".p$"
		args = append(args, rs.Arg("p", precision))
	}
	spec += "?}"
	return rs.Format(spec, args...)
}

// recording reports whether this run rewrites the oracle corpora.
func recording() bool { return os.Getenv("MJ_ORACLE") == "record" }

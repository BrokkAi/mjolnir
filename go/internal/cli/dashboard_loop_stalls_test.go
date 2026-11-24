// Port of mj-cli/src/dashboard/loop_stalls.rs tests (the platform-neutral
// one; the Linux scheduler test is in dashboard_loop_stalls_linux_test.go).

package cli

import (
	"testing"
	"time"
)

func TestStallsInsideTheIntervalAreHeldAndReportedWithTheNextLine(t *testing.T) {
	var limiter loopStallsLimiter
	start := time.Now()
	ms := func(ms int) time.Duration { return time.Duration(ms) * time.Millisecond }
	admit := func(at time.Duration, total time.Duration) (loopStallsSuppressed, bool) {
		return limiter.admit(start.Add(at), total)
	}
	expect := func(at time.Duration, total time.Duration, want *loopStallsSuppressed) {
		t.Helper()
		got, ok := admit(at, total)
		if want == nil {
			if ok {
				t.Fatalf("admit(+%v, %v) logged with %+v", at, total, got)
			}
			return
		}
		if !ok || got != *want {
			t.Fatalf("admit(+%v, %v) = %+v, %v; want %+v", at, total, got, ok, *want)
		}
	}
	expect(0, ms(150), &loopStallsSuppressed{})
	expect(ms(200), ms(400), nil)
	expect(ms(999), ms(120), nil)
	expect(ms(1_000), ms(110), &loopStallsSuppressed{count: 2, max: ms(400)})
	// The interval runs from the last line, not from the first stall.
	expect(ms(1_500), ms(100), nil)
	expect(ms(2_000), ms(100), &loopStallsSuppressed{count: 1, max: ms(100)})
}

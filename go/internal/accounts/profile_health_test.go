// Port of the tests in mj-accounts/src/profile_health.rs.

package accounts

import (
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func healthDiagnostic(message string, code *string, httpStatus *uint16) core.TurnDiagnostic {
	return core.TurnDiagnostic{Message: message, Code: code, HTTPStatus: httpStatus}
}

func healthStatus(status uint16) *uint16 { return &status }

func healthQuotaWindow(label string, remaining uint8, reset *int64) client.QuotaWindow {
	return client.QuotaWindow{Label: label, RemainingPercent: schedulerU8(remaining), ResetsAtEpochSeconds: reset}
}

func healthEpoch(seconds int64) time.Time { return time.Unix(seconds, 0) }

func healthUntil(t *testing.T, health *ProfileHealth, profileID, model string, at time.Time) time.Time {
	t.Helper()
	mark := health.Unhealthy(profileID, model, at)
	if mark == nil {
		t.Fatalf("no mark on %s/%s", profileID, model)
	}
	return mark.Until
}

// The diagnostics are the ones codex-acp's `sessionFailure` produced in the
// failures #1361 describes, as the daemon stored them.
func TestOnlyFailuresOfTheAccountMarkAProfile(t *testing.T) {
	fault := func(diagnostic core.TurnDiagnostic) *ProfileFault {
		found, ok := ProfileFaultOf(&diagnostic)
		if !ok {
			return nil
		}
		return &found
	}
	is := func(got *ProfileFault, want ProfileFault) bool { return got != nil && *got == want }
	rateLimit := healthDiagnostic("exceeded retry limit, last status: 429 Too Many Requests", schedulerText("limit"), nil)
	capacity := healthDiagnostic("Selected model is at capacity. Please try a different model.", schedulerText("service"), nil)
	if !is(fault(rateLimit), ProfileFaultRateLimited) {
		t.Fatal("rate limit")
	}
	if !is(fault(capacity), ProfileFaultProviderError) {
		t.Fatal("capacity")
	}
	if !is(fault(healthDiagnostic("You've hit your usage limit.", schedulerText("usageLimitExceeded"), nil)), ProfileFaultQuotaExhausted) {
		t.Fatal("usage limit")
	}
	if !is(fault(healthDiagnostic("overloaded", schedulerText("-32603"), healthStatus(529))), ProfileFaultProviderError) {
		t.Fatal("overloaded")
	}
	for _, notTheProfile := range []core.TurnDiagnostic{
		healthDiagnostic("invalid request body", schedulerText("request"), healthStatus(400)),
		healthDiagnostic("Forbidden", schedulerText("-32603"), healthStatus(403)),
		healthDiagnostic("Unauthorized", schedulerText("unauthorized"), healthStatus(401)),
		healthDiagnostic("stream disconnected", schedulerText("connection"), nil),
	} {
		if got := fault(notTheProfile); got != nil {
			t.Fatalf("%+v: %v", notTheProfile, *got)
		}
	}
}

func TestSimultaneousFailuresAreOneMarkAndRepeatsSoonAfterDoubleIt(t *testing.T) {
	failure := healthDiagnostic("429", schedulerText("limit"), nil)
	start := healthEpoch(1_000_000)
	health := &ProfileHealth{}
	record := func(at time.Time) *time.Time {
		health.Record("codex2", "sol", ProfileFaultRateLimited, &failure, at)
		mark := health.Unhealthy("codex2", "sol", at)
		if mark == nil {
			return nil
		}
		until := mark.Until
		return &until
	}
	first := record(start)
	if first == nil || !first.Equal(start.Add(healthBaseBackoff)) {
		t.Fatalf("first %v", first)
	}
	// Five sessions that were already running fail during the mark.
	for second := 1; second <= 5; second++ {
		if got := record(start.Add(time.Duration(second) * time.Second)); got == nil || !got.Equal(*first) {
			t.Fatalf("second %d: %v", second, got)
		}
	}
	if health.Unhealthy("codex2", "luna", start) != nil {
		t.Fatal("another model is marked")
	}
	if health.Unhealthy("codex2", "sol", *first) != nil {
		t.Fatal("the mark outlived its end")
	}
	again := first.Add(30 * time.Second)
	if got := record(again); got == nil || !got.Equal(again.Add(2*healthBaseBackoff)) {
		t.Fatalf("again %v", got)
	}
	afterQuiet := again.Add(2*healthBaseBackoff + healthQuietReset)
	if got := record(afterQuiet); got == nil || !got.Equal(afterQuiet.Add(healthBaseBackoff)) {
		t.Fatalf("after quiet %v", got)
	}
}

func TestProviderErrorsUseShortCappedBackoffWhileRateLimitsKeepTheirSchedule(t *testing.T) {
	start := healthEpoch(1_000_000)
	failure := healthDiagnostic("provider unavailable", schedulerText("service"), healthStatus(503))
	health := &ProfileHealth{}
	at := start
	for _, expected := range []int{8, 16, 32, 64, 64} {
		health.Record("codex2", "sol", ProfileFaultProviderError, &failure, at)
		until := healthUntil(t, health, "codex2", "sol", at)
		if !until.Equal(at.Add(time.Duration(expected) * time.Second)) {
			t.Fatalf("provider error backoff %v, want %ds", until.Sub(at), expected)
		}
		at = until.Add(time.Second)
	}

	rateLimit := healthDiagnostic("429", schedulerText("limit"), nil)
	health = &ProfileHealth{}
	at = start
	for _, expected := range []int{60, 120, 240, 480, 900, 900} {
		health.Record("codex2", "sol", ProfileFaultRateLimited, &rateLimit, at)
		until := healthUntil(t, health, "codex2", "sol", at)
		if !until.Equal(at.Add(time.Duration(expected) * time.Second)) {
			t.Fatalf("rate limit backoff %v, want %ds", until.Sub(at), expected)
		}
		at = until.Add(time.Second)
	}
}

func TestLaterSuccessClearsMarkAndNextFailureStartsAtFirstStrike(t *testing.T) {
	start := healthEpoch(1_000_000)
	failure := healthDiagnostic("429", schedulerText("limit"), nil)
	health := &ProfileHealth{}
	health.Record("codex2", "sol", ProfileFaultRateLimited, &failure, start)
	health.RecordSuccess("codex2", "sol", start.Add(time.Second))
	if health.Unhealthy("codex2", "sol", start) != nil {
		t.Fatal("the success did not clear the mark")
	}
	next := start.Add(2 * time.Second)
	health.Record("codex2", "sol", ProfileFaultRateLimited, &failure, next)
	if until := healthUntil(t, health, "codex2", "sol", next); !until.Equal(next.Add(healthBaseBackoff)) {
		t.Fatalf("until %v", until)
	}
}

func TestASuccessAfterAMarkEndsStillResetsItsStrikes(t *testing.T) {
	start := healthEpoch(1_000_000)
	failure := healthDiagnostic("429", schedulerText("limit"), nil)
	health := &ProfileHealth{}
	health.Record("codex2", "sol", ProfileFaultRateLimited, &failure, start)
	ended := start.Add(healthBaseBackoff + time.Second)
	if health.Unhealthy("codex2", "sol", ended) != nil {
		t.Fatal("the mark outlived its end")
	}
	if !health.HasMarks(ended) {
		t.Fatal("the strike still counts until the quiet reset")
	}
	if health.HasMarks(start.Add(healthBaseBackoff + healthQuietReset)) {
		t.Fatal("a mark past its quiet reset still counts")
	}

	health.Observe("codex2", "sol", TurnHealthSucceeded{CompletedAt: ended}, ended)
	next := ended.Add(time.Second)
	health.Observe("codex2", "sol", TurnHealthFailed{Fault: ProfileFaultRateLimited, Diagnostic: failure}, next)
	if until := healthUntil(t, health, "codex2", "sol", next); !until.Equal(next.Add(healthBaseBackoff)) {
		t.Fatalf("until %v", until)
	}
}

func TestOldSuccessDoesNotClearNewMark(t *testing.T) {
	start := healthEpoch(1_000_000)
	failure := healthDiagnostic("429", schedulerText("limit"), nil)
	health := &ProfileHealth{}
	health.Record("codex2", "sol", ProfileFaultRateLimited, &failure, start)
	health.RecordSuccess("codex2", "sol", start.Add(-time.Second))
	if until := healthUntil(t, health, "codex2", "sol", start); !until.Equal(start.Add(healthBaseBackoff)) {
		t.Fatalf("until %v", until)
	}
}

func TestClockResetTextSetsTheMarkEnd(t *testing.T) {
	parsed, _ := core.ParseChronoRFC3339("2026-09-21T16:50:00Z")
	_, offset := parsed.Local().Zone()
	now := parsed.In(time.FixedZone("", offset))
	failure := healthDiagnostic("429", schedulerText("limit"), nil)
	failure.ResetAt = schedulerText("23:00 UTC")
	expected, ok := NormalizeResetAt("23:00 UTC", now)
	if !ok {
		t.Fatal("normalize reset")
	}
	health := &ProfileHealth{}
	health.Record("codex2", "sol", ProfileFaultRateLimited, &failure, now)
	if until := healthUntil(t, health, "codex2", "sol", now); !until.Equal(expected) {
		t.Fatalf("until %v, want %v", until, expected)
	}
}

func TestExhaustedQuotaUsesReportedResetWithoutTheBackoffCap(t *testing.T) {
	now := healthEpoch(1_000_000)
	reset := int64(1_002_000)
	health := &ProfileHealth{}
	health.ObserveQuota("codex2", []client.QuotaWindow{healthQuotaWindow("Week", 0, &reset)}, now)
	failure := healthDiagnostic("quota exhausted", schedulerText("usageLimitExceeded"), nil)
	health.Record("codex2", "sol", ProfileFaultQuotaExhausted, &failure, now)
	if until := healthUntil(t, health, "codex2", "sol", now); !until.Equal(healthEpoch(reset)) {
		t.Fatalf("until %v", until)
	}
}

func TestScopedExhaustionHoldsOnlyTheMatchingModelMark(t *testing.T) {
	now := healthEpoch(1_000_000)
	reset := int64(1_002_000)
	fableWindow := healthQuotaWindow("Week (Fable)", 0, &reset)
	fableWindow.ModelScope = schedulerText("fable")
	health := &ProfileHealth{}
	health.ObserveQuota("claude", []client.QuotaWindow{fableWindow}, now)
	failure := healthDiagnostic("quota exhausted", schedulerText("usageLimitExceeded"), nil)
	health.Record("claude", "claude-fable-5-1", ProfileFaultQuotaExhausted, &failure, now)
	health.Record("claude", "claude-opus-5", ProfileFaultQuotaExhausted, &failure, now)

	if until := healthUntil(t, health, "claude", "claude-fable-5-1", now); !until.Equal(healthEpoch(reset)) {
		t.Fatalf("fable until %v", until)
	}
	if until := healthUntil(t, health, "claude", "claude-opus-5", now); !until.Equal(now.Add(healthBaseBackoff)) {
		t.Fatalf("opus until %v", until)
	}
}

func TestExhaustedQuotaWithoutAResetUsesTheFirstBackoff(t *testing.T) {
	now := healthEpoch(1_000_000)
	health := &ProfileHealth{}
	failure := healthDiagnostic("quota exhausted", schedulerText("usageLimitExceeded"), nil)
	health.Record("codex2", "sol", ProfileFaultQuotaExhausted, &failure, now)
	if until := healthUntil(t, health, "codex2", "sol", now); !until.Equal(now.Add(healthBaseBackoff)) {
		t.Fatalf("until %v", until)
	}
}

func TestQuotaFailureUsesResetFromMessage(t *testing.T) {
	parsed, _ := core.ParseChronoRFC3339("2026-09-21T16:50:00Z")
	now := parsed.UnixMilli()
	nowSystem := time.UnixMilli(now)
	message := "You've hit your quota; resets at 6:20pm (America/Chicago)"
	expected := MessageReset(message, now)
	if expected == nil {
		t.Fatal("message reset")
	}
	health := &ProfileHealth{}
	failure := healthDiagnostic(message, nil, healthStatus(402))
	health.Record("codex2", "sol", ProfileFaultQuotaExhausted, &failure, nowSystem)
	if until := healthUntil(t, health, "codex2", "sol", nowSystem); !until.Equal(healthEpoch(expected.At)) {
		t.Fatalf("until %v", until)
	}
}

func TestQuotaReportsOnlyLengthenActiveQuotaMarks(t *testing.T) {
	now := healthEpoch(1_000_000)
	failure := healthDiagnostic("failure", nil, healthStatus(402))
	health := &ProfileHealth{}
	health.Record("codex2", "quota", ProfileFaultQuotaExhausted, &failure, now)
	health.Record("codex2", "rate", ProfileFaultRateLimited, &failure, now)
	health.Record("codex2", "provider", ProfileFaultProviderError, &failure, now)
	longReset := int64(1_002_000)
	health.ObserveQuota("codex2", []client.QuotaWindow{healthQuotaWindow("Week", 0, &longReset)}, now)
	if until := healthUntil(t, health, "codex2", "quota", now); !until.Equal(healthEpoch(longReset)) {
		t.Fatalf("quota until %v", until)
	}
	shorterReset := int64(1_001_000)
	health.ObserveQuota("codex2", []client.QuotaWindow{healthQuotaWindow("Week", 0, &shorterReset)}, now)
	if until := healthUntil(t, health, "codex2", "quota", now); !until.Equal(healthEpoch(longReset)) {
		t.Fatalf("quota until after a shorter reset %v", until)
	}
	if until := healthUntil(t, health, "codex2", "rate", now); !until.Equal(now.Add(healthBaseBackoff)) {
		t.Fatalf("rate until %v", until)
	}
	if until := healthUntil(t, health, "codex2", "provider", now); !until.Equal(now.Add(healthProviderErrorBaseBackoff)) {
		t.Fatalf("provider until %v", until)
	}
}

func TestWindowsWithQuotaLeftDoNotHoldAQuotaMark(t *testing.T) {
	now := healthEpoch(1_000_000)
	failure := healthDiagnostic("quota exhausted", schedulerText("usageLimitExceeded"), nil)
	health := &ProfileHealth{}
	reset := int64(1_014_400)
	health.ObserveQuota("codex2", []client.QuotaWindow{healthQuotaWindow("5h", 30, &reset)}, now)
	health.Record("codex2", "sol", ProfileFaultQuotaExhausted, &failure, now)
	if until := healthUntil(t, health, "codex2", "sol", now); !until.Equal(now.Add(healthBaseBackoff)) {
		t.Fatalf("until %v", until)
	}
	health.ObserveQuota("codex2", []client.QuotaWindow{healthQuotaWindow("5h", 30, &reset)}, now)
	if until := healthUntil(t, health, "codex2", "sol", now); !until.Equal(now.Add(healthBaseBackoff)) {
		t.Fatalf("until after the report %v", until)
	}
}

func TestRateLimitIgnoresStoredQuotaWindows(t *testing.T) {
	now := healthEpoch(1_000_000)
	health := &ProfileHealth{}
	reset := int64(2_000_000)
	health.ObserveQuota("codex2", []client.QuotaWindow{healthQuotaWindow("Week", 0, &reset)}, now)
	failure := healthDiagnostic("429", nil, healthStatus(429))
	health.Record("codex2", "sol", ProfileFaultRateLimited, &failure, now)
	if until := healthUntil(t, health, "codex2", "sol", now); !until.Equal(now.Add(healthBaseBackoff)) {
		t.Fatalf("until %v", until)
	}
}

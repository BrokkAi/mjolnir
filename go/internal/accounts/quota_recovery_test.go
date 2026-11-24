// Port of the tests in mj-accounts/src/quota/recovery.rs.

package accounts

import (
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
)

func recoveryScoringWindow(remaining uint8, reset, duration *int64) client.QuotaWindow {
	return client.QuotaWindow{
		Label:                "window",
		RemainingPercent:     &remaining,
		DurationSeconds:      duration,
		ResetsAtEpochSeconds: reset,
	}
}

func recoveryWindow(label string, remaining uint8, reset *int64) client.QuotaWindow {
	return client.QuotaWindow{Label: label, RemainingPercent: &remaining, ResetsAtEpochSeconds: reset}
}

func recoveryAt(value int64) *int64 { return &value }

func recoveryRFC3339(t *testing.T, text string) time.Time {
	t.Helper()
	parsed, err := time.Parse(time.RFC3339, text)
	if err != nil {
		t.Fatal(err)
	}
	return parsed
}

func recoveryScoreIs(t *testing.T, windows []client.QuotaWindow, model string, now int64, want float64) {
	t.Helper()
	if score, ok := QuotaScore(windows, model, now); !ok || score != want {
		t.Fatalf("score = %v, %t; want %v", score, ok, want)
	}
}

func TestQuotaScoreAccountsForResetPacingAndScopedWindows(t *testing.T) {
	now := int64(1_000_000)
	short := recoveryScoringWindow(5, recoveryAt(now+180), recoveryAt(18_000))
	weekly := recoveryScoringWindow(5, recoveryAt(now+6*24*60*60), recoveryAt(604_800))
	shortScore, ok := QuotaScore([]client.QuotaWindow{short}, "claude-opus-5", now)
	if !ok {
		t.Fatal("the short window scores")
	}
	weeklyScore, ok := QuotaScore([]client.QuotaWindow{weekly}, "claude-opus-5", now)
	if !ok {
		t.Fatal("the weekly window scores")
	}
	if shortScore != 4.0 {
		t.Fatalf("short score = %v", shortScore)
	}
	if !(shortScore > weeklyScore) {
		t.Fatalf("short %v <= weekly %v", shortScore, weeklyScore)
	}

	recoveryScoreIs(t, []client.QuotaWindow{recoveryScoringWindow(0, recoveryAt(now+5), recoveryAt(10))}, "x", now, 0.0)
	recoveryScoreIs(t, []client.QuotaWindow{recoveryScoringWindow(20, recoveryAt(now), recoveryAt(10))}, "x", now, 1.0)
	recoveryScoreIs(t, []client.QuotaWindow{recoveryScoringWindow(0, recoveryAt(now), recoveryAt(10))}, "x", now, 1.0)
	recoveryScoreIs(t, []client.QuotaWindow{recoveryScoringWindow(25, nil, nil)}, "x", now, 0.5)
	usage := recoveryScoringWindow(0, nil, nil)
	usage.RemainingPercent = nil
	usage.Used = recoveryAt(75)
	usage.Limit = recoveryAt(100)
	recoveryScoreIs(t, []client.QuotaWindow{usage}, "x", now, 0.5)
	recoveryScoreIs(t, []client.QuotaWindow{short, weekly}, "x", now, weeklyScore)

	scoped := recoveryScoringWindow(0, recoveryAt(now+10), recoveryAt(10))
	scope := "fable"
	scoped.ModelScope = &scope
	if score, ok := QuotaScore([]client.QuotaWindow{scoped}, "claude-opus-5", now); ok {
		t.Fatalf("a Fable window scores an Opus model: %v", score)
	}
	recoveryScoreIs(t, []client.QuotaWindow{scoped}, "CLAUDE-FABLE-5-1", now, 0.0)
}

func recoveryResetIs(t *testing.T, got *int64, want *int64) {
	t.Helper()
	if (got == nil) != (want == nil) || (got != nil && *got != *want) {
		t.Fatalf("reset = %v; want %v", recoveryText(got), recoveryText(want))
	}
}

func recoveryText(value *int64) any {
	if value == nil {
		return "None"
	}
	return *value
}

func TestWaitsForEveryExhaustedWindowAndNeverReusesAReset(t *testing.T) {
	windows := []client.QuotaWindow{recoveryWindow("5H", 0, recoveryAt(500)), recoveryWindow("Week", 0, recoveryAt(900))}
	recoveryResetIs(t, RecoveryReset(windows, nil, 100, nil), recoveryAt(900))
	recoveryResetIs(t, RecoveryReset(windows, nil, 600, recoveryAt(500)), recoveryAt(900))
	windows[1].RemainingPercent = func() *uint8 { value := uint8(50); return &value }()
	recoveryResetIs(t, RecoveryReset(windows, nil, 100, nil), recoveryAt(500))
	recoveryResetIs(t, RecoveryReset(windows, nil, 100, recoveryAt(500)), nil)
	windows[0].ResetsAtEpochSeconds = nil
	recoveryResetIs(t, RecoveryReset(windows, nil, 100, nil), nil)
	recoveryResetIs(t, RecoveryReset(windows, recoveryAt(700), 100, nil), recoveryAt(700))
	windows[0].RemainingPercent = func() *uint8 { value := uint8(20); return &value }()
	recoveryResetIs(t, RecoveryReset(windows, nil, 100, nil), recoveryAt(900))
}

func TestLongWindowDecisionRespectsScopeUnknownResetsAndFiveHourBoundary(t *testing.T) {
	now := int64(1_000_000)
	short := recoveryWindow("5H", 0, recoveryAt(now+18_000))
	weekly := recoveryWindow("Week", 0, recoveryAt(now+18_001))
	scoped := recoveryWindow("Week (Fable)", 0, nil)
	scope := "fable"
	scoped.ModelScope = &scope
	duration := recoveryWindow("Allowance", 0, nil)
	duration.DurationSeconds = recoveryAt(604_800)
	for _, decision := range []struct {
		name     string
		windows  []client.QuotaWindow
		model    string
		explicit *int64
		expected bool
	}{
		{"exactly five hours", []client.QuotaWindow{recoveryWindow("Week", 0, recoveryAt(now+18_000))}, "x", nil, false},
		{"distant weekly", []client.QuotaWindow{weekly}, "x", nil, true},
		{"short only", []client.QuotaWindow{short}, "x", nil, false},
		{"unknown weekly", []client.QuotaWindow{recoveryWindow("Week", 0, nil)}, "x", nil, true},
		{"both exhausted", []client.QuotaWindow{short, weekly}, "x", nil, true},
		{"scoped applies", []client.QuotaWindow{scoped}, "claude-fable", nil, true},
		{"scoped excludes", []client.QuotaWindow{scoped}, "claude-opus", nil, false},
		{"duration", []client.QuotaWindow{duration}, "x", nil, true},
		{"explicit only", nil, "x", recoveryAt(now + 18_001), true},
		{"explicit boundary", nil, "x", recoveryAt(now + 18_000), false},
	} {
		var applicable []client.QuotaWindow
		for i := range decision.windows {
			if WindowApplies(&decision.windows[i], decision.model) {
				applicable = append(applicable, decision.windows[i])
			}
		}
		reset := RecoveryReset(applicable, decision.explicit, now, nil)
		if got := LongWindowStop(applicable, decision.model, decision.explicit, reset, now); got != decision.expected {
			t.Fatalf("%s: %t", decision.name, got)
		}
	}
}

func recoveryStatedIs(t *testing.T, got *StatedReset, want *StatedReset, message string) {
	t.Helper()
	if (got == nil) != (want == nil) || (got != nil && *got != *want) {
		t.Fatalf("%s: stated = %+v; want %+v", message, got, want)
	}
}

func TestMessageClockUsesNamedZoneAndRejectsDstAmbiguity(t *testing.T) {
	observed := recoveryRFC3339(t, "2026-09-21T16:50:00Z").UnixMilli()
	recoveryStatedIs(t,
		MessageReset("You've hit your session limit · resets 1:20pm (America/Chicago)", observed),
		&StatedReset{At: recoveryRFC3339(t, "2026-09-21T18:20:00Z").Unix(), Relative: false},
		"a named zone")
	fall := recoveryRFC3339(t, "2026-11-01T04:00:00Z").UnixMilli()
	recoveryStatedIs(t, MessageReset("resets 1:30am (America/Chicago)", fall), nil, "an ambiguous clock")
	spring := recoveryRFC3339(t, "2026-03-08T04:00:00Z").UnixMilli()
	recoveryStatedIs(t, MessageReset("resets 2:30am (America/Chicago)", spring), nil, "a skipped clock")
	recoveryStatedIs(t, MessageReset("resets 1pm (Invalid/Zone)", observed), nil, "an unknown zone")
}

// Hard-won: #1555: Antigravity states a quota reset as a duration from the
// refusal, which was unreadable, so its five-hour reset was overlooked and
// the weekly window's days-away reset was the wait recorded for the session.
func TestAntigravityDurationResetBecomesTheRetryInsteadOfTheWeeklyWindow(t *testing.T) {
	observed := recoveryRFC3339(t, "2026-11-16T23:32:00Z").UnixMilli()
	now := observed / 1000
	refusal := "⚠ Individual quota reached. Please upgrade your subscription to " +
		"increase your limits. Resets in 40s."
	stated := MessageReset(refusal, observed)
	if stated == nil || stated.At-now != 40 {
		t.Fatalf("the short limit's reset is a duration from the refusal: %+v", stated)
	}
	// The same harness states a long allowance with compound units, and a
	// line without a reset says nothing about a deadline.
	if long := MessageReset("⚠ Individual quota reached. Resets in 102h19m11s.", observed); long == nil || long.At-now != 102*3_600+19*60+11 {
		t.Fatalf("compound units: %+v", long)
	}
	recoveryStatedIs(t, MessageReset("You have used some of your 5-hour limit", observed), nil, "no reset")
	// A wall-clock reset is not a duration, so nothing here is relative.
	recoveryStatedIs(t,
		MessageReset("Quota reached; resets at 2026-11-17T02:00:00Z", observed),
		&StatedReset{At: recoveryRFC3339(t, "2026-11-17T02:00:00Z").Unix(), Relative: false},
		"a wall-clock reset")
	// The report the daemon already has is the one #1555 saw: the weekly
	// allowance exhausted with a reset six days out, and the five-hour
	// allowance with quota left, so its own reset says nothing. Only the
	// message names the limit that actually refused the turn.
	fiveHour := recoveryWindow("5H", 55, recoveryAt(now+3_600))
	fiveHour.DurationSeconds = recoveryAt(5 * 3_600)
	weekly := recoveryWindow("Week", 0, recoveryAt(now+6*86_400))
	windows := []client.QuotaWindow{fiveHour, weekly}
	recoveryResetIs(t, RetryDeadline(windows, stated, now, nil), recoveryAt(now+40))
	// Each refusal is decided on what that refusal reported, and each
	// attempt is spaced by the reset it names, so repeated refusals pace
	// themselves instead of spinning. A longer wait from a later limit is
	// honored too.
	repeated := MessageReset(refusal, (now+100)*1_000)
	recoveryResetIs(t, RetryDeadline(windows, repeated, now+100, recoveryAt(now+40)), recoveryAt(now+140))
	// The same reset already spent by the refused retry cannot schedule
	// another attempt at that instant.
	recoveryResetIs(t, RetryDeadline(windows, stated, now+100, recoveryAt(now+40)), recoveryAt(now+6*86_400))
	longer := MessageReset("resets in 2h", (now+100)*1_000)
	recoveryResetIs(t, RetryDeadline(windows, longer, now+100, recoveryAt(now+40)), recoveryAt(now+100+2*3_600))
	// A deadline already in the past has arrived, so the retry is due now
	// rather than at whatever reset an earlier report names.
	recoveryResetIs(t, RetryDeadline(windows, stated, now+200, nil), recoveryAt(now+200))
	// An available allowance's earlier reset never shortens the deadline
	// the message gave: nothing says that allowance refused the turn.
	availableSoon := []client.QuotaWindow{recoveryWindow("Week", 80, recoveryAt(now+600))}
	recoveryResetIs(t, RetryDeadline(availableSoon, stated, now, nil), recoveryAt(now+40))
	recoveryResetIs(t, RetryDeadline(availableSoon, &StatedReset{At: now + 7_200, Relative: false}, now, nil), recoveryAt(now+7_200))
	// With no message the report alone decides: the furthest reset of the
	// exhausted allowances, or the earliest reset there is.
	reported := []client.QuotaWindow{
		recoveryWindow("5H", 0, recoveryAt(now+600)),
		recoveryWindow("Week", 0, recoveryAt(now+6*86_400)),
	}
	recoveryResetIs(t, RetryDeadline(reported, nil, now, nil), recoveryAt(now+6*86_400))
	recoveryResetIs(t, RetryDeadline(availableSoon, nil, now, nil), recoveryAt(now+600))
	// An absolute statement is used once: retrying at that time leaves it
	// in the past, and a report that cannot name a reset schedules none.
	absolute := MessageReset("resets at 2026-11-17T02:00:00Z", observed)
	absoluteAt := now + 2*3_600 + 28*60
	recoveryResetIs(t, RetryDeadline(nil, absolute, now, nil), recoveryAt(absoluteAt))
	recoveryResetIs(t, RetryDeadline(nil, absolute, now+100, recoveryAt(absoluteAt)), nil)
	recoveryResetIs(t, RetryDeadline(nil, nil, now, nil), nil)
}

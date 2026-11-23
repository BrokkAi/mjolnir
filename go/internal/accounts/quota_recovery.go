// Port of mj-accounts/src/quota/recovery.rs.

// Deadline calculation never asks a model to interpret a clock or timezone.

package accounts

import (
	"github.com/BrokkAi/mjolnir/go/internal/client"
)

// WindowApplies is whether a scoped quota window applies to the selected
// model.
func WindowApplies(window *client.QuotaWindow, model string) bool {
	return window.AppliesToModel(model)
}

// QuotaScore scores quota headroom against its reset deadline. Higher
// scores mean more sustainable capacity for the requested model. False when
// no window that applies to the model says how much is left.
func QuotaScore(windows []client.QuotaWindow, model string, nowSeconds int64) (float64, bool) {
	panic("unported: mj-accounts/src/quota/recovery.rs quota_score")
}

// MergeResetWindows re-exports client.MergeResetWindows (Rust's
// `pub use mj_client::quota::merge_reset_windows`).
func MergeResetWindows(windows *[]client.QuotaWindow, cached []client.QuotaWindow) bool {
	return client.MergeResetWindows(windows, cached)
}

// StatedReset is a reset deadline a provider stated in its own message, and
// whether it was stated as a duration from the refusal (Antigravity:
// "Resets in 40s"). A relative statement is only true of the moment it was
// read: repeating it after a retry says nothing new about the deadline.
type StatedReset struct {
	At       int64
	Relative bool
}

func MessageReset(message string, observedMs int64) *StatedReset {
	panic("unported: mj-accounts/src/quota/recovery.rs message_reset")
}

// durationSeconds is the seconds in a duration stated in words, as in
// Antigravity's "Resets in 40s" or "Resets in 102h19m11s". Anything else
// (`at 14:00`, `Aug 14 at 4am`, `2026-11-12T19:52:53Z`) is not a duration and
// the caller keeps its wall-clock parsing.
func durationSeconds(value string) (int64, bool) {
	panic("unported: mj-accounts/src/quota/recovery.rs duration_seconds")
}

// Exhausted is whether window shows no quota left and has not reset by
// nowSeconds.
func Exhausted(window *client.QuotaWindow, nowSeconds int64) bool {
	if window.ResetsAtEpochSeconds != nil && *window.ResetsAtEpochSeconds <= nowSeconds {
		return false
	}
	if window.RemainingPercent != nil && *window.RemainingPercent == 0 {
		return true
	}
	return window.Used != nil && window.Limit != nil && *window.Limit > 0 && *window.Used >= *window.Limit
}

// LongWindowStop: only a long allowance with a distant or unknown
// continuation time merits a Move.
func LongWindowStop(windows []client.QuotaWindow, model string, explicit *int64, reset *int64, nowSeconds int64) bool {
	const fiveHours = 5 * 60 * 60
	anyExhausted, weeklyExhausted := false, false
	for i := range windows {
		window := &windows[i]
		if WindowApplies(window, model) && Exhausted(window, nowSeconds) {
			anyExhausted = true
			weeklyExhausted = weeklyExhausted || window.IsWeeklyOrLonger()
		}
	}
	if !anyExhausted {
		return explicit != nil && quotaSaturatingSubInt64(*explicit, nowSeconds) > fiveHours
	}
	return weeklyExhausted && (reset == nil || quotaSaturatingSubInt64(*reset, nowSeconds) > fiveHours)
}

// RecoveryReset: a reset already used for an unsuccessful retry cannot
// schedule another retry.
func RecoveryReset(windows []client.QuotaWindow, explicit *int64, nowSeconds int64, consumed *int64) *int64 {
	panic("unported: mj-accounts/src/quota/recovery.rs recovery_reset")
}

// RetryDeadline is the deadline for the next attempt after a refusal.
//
// A provider's message names the limit that just refused the turn; its quota
// report says which allowances are out overall, and those are not always the
// same limit (Antigravity refused a turn on its five-hour allowance while the
// report showed only the weekly one exhausted, days away — #1555). So a
// stated deadline decides the wait, and the report decides it only when the
// message states none: an exhausted allowance's furthest reset, or the
// earliest reset otherwise. An available allowance's reset never shortens
// the wait, because nothing says that allowance is what refused the turn.
//
// Every refusal is decided on what that refusal reported, including one that
// repeats an earlier duration: the provider is the owner of its own limit,
// and the only thing distinguishing a repeated statement from a new limit is
// a guess this code cannot check. Waiting is bounded by that report either
// way, since each attempt is spaced by the reset it names plus a minute, so
// repeated refusals pace themselves instead of spinning. A deadline already
// in the past has arrived, so the retry is due now rather than at a reset
// read earlier.
func RetryDeadline(windows []client.QuotaWindow, stated *StatedReset, nowSeconds int64, consumed *int64) *int64 {
	if stated != nil && (consumed == nil || stated.At > *consumed) {
		at := max(stated.At, nowSeconds)
		return &at
	}
	return RecoveryReset(windows, nil, nowSeconds, consumed)
}

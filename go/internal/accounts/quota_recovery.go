// Port of mj-accounts/src/quota/recovery.rs.

// Deadline calculation never asks a model to interpret a clock or timezone.

package accounts

import (
	"math"
	"strconv"
	"strings"
	"time"
	"unicode"

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
	var lowest float64
	scored := false
	for i := range windows {
		window := &windows[i]
		if !WindowApplies(window, model) {
			continue
		}
		var remaining float64
		switch {
		case window.RemainingPercent != nil:
			remaining = float64(*window.RemainingPercent)
		case window.Used != nil && window.Limit != nil && *window.Limit > 0:
			limit := float64(*window.Limit)
			remaining = min(max(100*(limit-float64(*window.Used))/limit, 0), 100)
		default:
			continue
		}
		remaining = min(remaining, 100)
		var score float64
		switch {
		case window.ResetsAtEpochSeconds != nil && *window.ResetsAtEpochSeconds <= nowSeconds:
			score = 1
		case remaining == 0:
			score = 0
		case window.ResetsAtEpochSeconds != nil && window.DurationSeconds != nil && *window.DurationSeconds > 0:
			duration := *window.DurationSeconds
			secondsLeft := min(max(quotaSaturatingSubInt64(*window.ResetsAtEpochSeconds, nowSeconds), 0), duration)
			pace := 100 * float64(secondsLeft) / float64(duration)
			if pace <= recoveryEpsilon {
				score = 4
			} else {
				score = min(remaining/pace, 4)
			}
		default:
			score = min(remaining/50, 4)
		}
		if !scored || score < lowest {
			lowest = score
		}
		scored = true
	}
	return lowest, scored
}

// recoveryEpsilon is Rust's f64::EPSILON.
const recoveryEpsilon = 2.220446049250313e-16

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
	observed := time.UnixMilli(observedMs)
	if observed.Unix() < quotaChronoMinTimestamp || observed.Unix() > quotaChronoMaxTimestamp {
		return nil
	}
	now := quotaFixed(observed.In(time.Local))
	// ASCII lowercasing keeps every byte offset, so the index reads the
	// original message.
	index := strings.Index(quotaASCIILower(message), "reset")
	if index < 0 {
		return nil
	}
	value := strings.TrimSpace(strings.TrimLeft(message[index+len("reset"):], "s"))
	value = strings.TrimPrefix(value, "at ")
	value = strings.TrimSpace(strings.TrimLeft(value, ":"))
	if value == "" {
		return nil
	}
	line, _, _ := strings.Cut(value, "\n")
	line = strings.TrimSuffix(line, "\r")
	value = strings.TrimRight(strings.TrimSpace(line), ".")
	// Some providers name the wait as a duration from the refusal
	// (Antigravity: "Resets in 40s") rather than a wall-clock time.
	if seconds, ok := durationSeconds(value); ok {
		if now.Unix() > math.MaxInt64-seconds {
			return nil
		}
		return &StatedReset{At: now.Unix() + seconds, Relative: true}
	}
	at, ok := NormalizeResetAt(value, now)
	if !ok {
		return nil
	}
	return &StatedReset{At: at.Unix(), Relative: false}
}

// durationSeconds is the seconds in a duration stated in words, as in
// Antigravity's "Resets in 40s" or "Resets in 102h19m11s". Anything else
// (`at 14:00`, `Aug 14 at 4am`, `2026-11-12T19:52:53Z`) is not a duration and
// the caller keeps its wall-clock parsing.
func durationSeconds(value string) (int64, bool) {
	value = strings.TrimSpace(value)
	if rest, ok := strings.CutPrefix(value, "in "); ok {
		value = strings.TrimLeftFunc(rest, unicode.IsSpace)
	}
	chars := []rune(value)
	at := 0
	var total int64
	stated := false
	for {
		for at < len(chars) && (unicode.IsSpace(chars[at]) || chars[at] == ',') {
			at++
		}
		if at == len(chars) {
			break
		}
		start := at
		for at < len(chars) && chars[at] >= '0' && chars[at] <= '9' {
			at++
		}
		amount := string(chars[start:at])
		for at < len(chars) && unicode.IsSpace(chars[at]) {
			at++
		}
		start = at
		for at < len(chars) && (chars[at] >= 'a' && chars[at] <= 'z' || chars[at] >= 'A' && chars[at] <= 'Z') {
			at++
		}
		unit := string(chars[start:at])
		if amount == "" || unit == "" {
			return 0, false
		}
		var unitSeconds int64
		switch quotaASCIILower(unit) {
		case "s", "sec", "secs", "second", "seconds":
			unitSeconds = 1
		case "m", "min", "mins", "minute", "minutes":
			unitSeconds = 60
		case "h", "hr", "hrs", "hour", "hours":
			unitSeconds = 60 * 60
		case "d", "day", "days":
			unitSeconds = 24 * 60 * 60
		case "w", "wk", "wks", "week", "weeks":
			unitSeconds = 7 * 24 * 60 * 60
		default:
			return 0, false
		}
		parsed, err := strconv.ParseInt(amount, 10, 64)
		if err != nil || parsed > math.MaxInt64/unitSeconds {
			return 0, false
		}
		seconds := parsed * unitSeconds
		if total > math.MaxInt64-seconds {
			return 0, false
		}
		total += seconds
		stated = true
	}
	return total, stated
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
	usable := func(time *int64) bool {
		return time != nil && *time > nowSeconds && (consumed == nil || *time > *consumed)
	}
	anyExhausted := false
	var furthest int64
	for i := range windows {
		window := &windows[i]
		if !Exhausted(window, nowSeconds) {
			continue
		}
		// Unknown exhausted windows must not be silently ignored. A
		// provider's explicit reset can supply the missing bound.
		var reset int64
		switch {
		case usable(window.ResetsAtEpochSeconds):
			reset = *window.ResetsAtEpochSeconds
		case usable(explicit):
			reset = *explicit
		default:
			return nil
		}
		if !anyExhausted || reset > furthest {
			furthest = reset
		}
		anyExhausted = true
	}
	if anyExhausted {
		return &furthest
	}
	if usable(explicit) {
		reset := *explicit
		return &reset
	}
	var earliest *int64
	for i := range windows {
		if reset := windows[i].ResetsAtEpochSeconds; usable(reset) && (earliest == nil || *reset < *earliest) {
			value := *reset
			earliest = &value
		}
	}
	return earliest
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

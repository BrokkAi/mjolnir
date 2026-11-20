// Port of mj-client/src/quota.rs.

// Quota data and display helpers shared by Mjolnir's control surfaces.

package client

import (
	"encoding/json"
	"errors"
	"fmt"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// AntigravityQuotaPool is core's pool (Rust re-exports it here).
type AntigravityQuotaPool = core.AntigravityQuotaPool

// APILabel is the label used when a harness is billed by API usage rather
// than a subscription.
const APILabel = "API"

// NotReportedLabel is the label used when a harness never shares its quota
// with any client, so there is no reading to take and none is missing.
const NotReportedLabel = "n/a"

// IdleLabel is the label used when the subscription is active but no usage
// window is open and the provider reports no usage at all until the next
// request opens one, so even the long window is unknown. Muse's quota
// endpoint does this.
const IdleLabel = "idle"

// AntigravityPoolOfWindow is the Antigravity pool a window's model scope
// names, if any.
func AntigravityPoolOfWindow(window *QuotaWindow) (AntigravityQuotaPool, bool) {
	if window.ModelScope == nil {
		return 0, false
	}
	return core.AntigravityQuotaPoolOfScope(*window.ModelScope)
}

// QuotaWindow is a quota window reported by a harness.
type QuotaWindow struct {
	Label            string `json:"label"`
	RemainingPercent *uint8 `json:"remaining_percent"`
	DurationSeconds  *int64 `json:"duration_seconds,omitempty"`
	// ModelScope is model-name fragments sharing this allowance, separated
	// by commas. A fragment written with a leading `!` leaves out models
	// containing it.
	ModelScope           *string `json:"model_scope,omitempty"`
	Used                 *int64  `json:"used"`
	Limit                *int64  `json:"limit"`
	Resets               *string `json:"resets"`
	ResetsAtEpochSeconds *int64  `json:"resets_at_epoch_seconds"`
}

func (w *QuotaWindow) UnmarshalJSON(data []byte) error {
	type plain QuotaWindow
	return feedDecodeStruct(data, (*plain)(w), false, "label")
}

// AppliesToModel is whether this window's allowance covers model.
func (w *QuotaWindow) AppliesToModel(model string) bool {
	return w.ModelScope == nil || core.ModelMatchesScope(model, *w.ModelScope)
}

// IsWeeklyOrLonger is whether this allowance spans a week or longer,
// including model-scoped labels.
func (w *QuotaWindow) IsWeeklyOrLonger() bool {
	label := w.Label
	if w.ModelScope != nil {
		if before, _, found := strings.Cut(label, " ("); found {
			label = before
		}
	}
	return isWeeklyQuotaWindow(label) ||
		(w.DurationSeconds != nil && *w.DurationSeconds >= 7*24*60*60)
}

// QuotaSnapshot is what the daemon publishes about quota. The daemon is the
// only process that asks a provider for quota; every surface reads this
// instead.
type QuotaSnapshot struct {
	// Reports is the latest report for each enabled profile.
	Reports map[string]ProfileQuota `json:"reports"`
	// Probing is the profiles the daemon is asking a provider about right
	// now.
	Probing sortedStringSet `json:"probing"`
	// Cycles is the probe cycles the daemon has finished. A surface that
	// asked for a refresh remembers the value it saw and knows the refresh
	// is over when this grows.
	Cycles uint64 `json:"cycles"`
}

func (s QuotaSnapshot) MarshalJSON() ([]byte, error) {
	type plain QuotaSnapshot
	value := plain(s)
	if value.Reports == nil {
		value.Reports = map[string]ProfileQuota{}
	}
	if value.Probing == nil {
		value.Probing = sortedStringSet{}
	}
	return feedMarshalJSON(value)
}

func (s *QuotaSnapshot) UnmarshalJSON(data []byte) error {
	type plain QuotaSnapshot
	return feedDecodeStruct(data, (*plain)(s), false, "reports", "probing", "cycles")
}

// ProfileQuota is the quota report shown for one harness profile.
type ProfileQuota struct {
	// MonthlyPriceUsd is the monthly USD list price used as a subscription
	// capacity proxy.
	MonthlyPriceUsd *uint32 `json:"monthly_price_usd,omitempty"`
	// PaidCredits is paid extra usage, independent of included quota and
	// banked resets.
	PaidCredits *PaidCredits `json:"paid_credits,omitempty"`
	// BankedResets is unused provider-granted resets, absent when the
	// provider cannot report them.
	BankedResets            *uint64          `json:"banked_resets,omitempty"`
	ProfileID               string           `json:"profile_id"`
	Harness                 core.HarnessKind `json:"harness"`
	Windows                 []QuotaWindow    `json:"windows"`
	Extra                   *string          `json:"extra"`
	Error                   *string          `json:"error"`
	RefreshedAtEpochSeconds uint64           `json:"refreshed_at_epoch_seconds"`
	// RateLimitedUntilEpochSeconds: the provider answered 429 (too many
	// requests): the daemon will not probe this profile again before this
	// time. The windows, when there are any, are the last good reading, and
	// RefreshedAtEpochSeconds is when it was taken.
	RateLimitedUntilEpochSeconds *uint64 `json:"rate_limited_until_epoch_seconds,omitempty"`
	// NextCheckAtEpochSeconds: with a hold, when the daemon next asks the
	// provider, one regular interval after the refused attempt or the end of
	// the hold, whichever is later. Readers that predate it fall back to the
	// hold's end.
	NextCheckAtEpochSeconds *uint64 `json:"next_check_at_epoch_seconds,omitempty"`
}

func (q ProfileQuota) MarshalJSON() ([]byte, error) {
	type plain ProfileQuota
	value := plain(q)
	if value.Windows == nil {
		value.Windows = []QuotaWindow{}
	}
	return feedMarshalJSON(value)
}

func (q *ProfileQuota) UnmarshalJSON(data []byte) error {
	type plain ProfileQuota
	return feedDecodeStruct(data, (*plain)(q), false,
		"profile_id", "harness", "windows", "refreshed_at_epoch_seconds")
}

// PaidCreditsKind is which paid funds a provider reports.
type PaidCreditsKind int

const (
	// PaidCreditsLimited is remaining funds, constrained by any applicable
	// spending cap.
	PaidCreditsLimited PaidCreditsKind = iota
	// PaidCreditsUnlimited: the provider explicitly reports unrestricted
	// paid usage.
	PaidCreditsUnlimited
	// PaidCreditsAvailable: the provider confirms funds but does not
	// disclose their amount.
	PaidCreditsAvailable
)

// PaidCredits is provider-native paid funds usable after included
// subscription quota ends. Adjacently tagged: {"kind":"limited","balance":n},
// {"kind":"unlimited"}, {"kind":"available"}.
type PaidCredits struct {
	Kind PaidCreditsKind
	// Balance is the Limited balance as serde_json::Number holds it: a
	// jsonvalue number (int64, uint64 or float64).
	Balance any
}

// NewPaidCreditsLimited is Rust's PaidCredits::limited: a finite balance,
// with a negative one read as zero; nil for NaN or an infinity.
func NewPaidCreditsLimited(balance float64) *PaidCredits {
	panic("unported: mj-client/src/quota.rs PaidCredits::limited")
}

func (c PaidCredits) MarshalJSON() ([]byte, error) {
	switch c.Kind {
	case PaidCreditsLimited:
		balance, err := jsonvalue.Marshal(c.Balance)
		if err != nil {
			return nil, err
		}
		return []byte(`{"kind":"limited","balance":` + string(balance) + `}`), nil
	case PaidCreditsUnlimited:
		return []byte(`{"kind":"unlimited"}`), nil
	case PaidCreditsAvailable:
		return []byte(`{"kind":"available"}`), nil
	}
	return nil, fmt.Errorf("unknown paid credits kind %d", c.Kind)
}

func (c *PaidCredits) UnmarshalJSON(data []byte) error {
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return err
	}
	rawKind, ok := members["kind"]
	if !ok {
		return errors.New("missing field `kind`")
	}
	var kind string
	if err := json.Unmarshal(rawKind, &kind); err != nil {
		return err
	}
	switch kind {
	case "limited":
		content, ok := members["balance"]
		if !ok {
			return errors.New("missing field `balance`")
		}
		balance, err := jsonvalue.Parse(content)
		if err != nil {
			return err
		}
		switch balance.(type) {
		case int64, uint64, float64:
		default:
			return fmt.Errorf("invalid type: %s, expected a JSON number", content)
		}
		*c = PaidCredits{Kind: PaidCreditsLimited, Balance: balance}
	case "unlimited":
		*c = PaidCredits{Kind: PaidCreditsUnlimited}
	case "available":
		*c = PaidCredits{Kind: PaidCreditsAvailable}
	default:
		return fmt.Errorf("unknown variant `%s`, expected one of `limited`, `unlimited`, `available`", kind)
	}
	return nil
}

// RefusedLoginMessage is actionable refusal text shared by profile, doctor,
// and session views.
func RefusedLoginMessage(profileID string) string {
	return fmt.Sprintf("login refused; automatic renewal failed; run `mj login --profile %s`", profileID)
}

// RetainAntigravityPools is what people see of an Antigravity report: only
// the windows of pools, the pools the profile uses. The stored report keeps
// every pool for selection and quota recovery. Other harnesses are
// unchanged.
func (q *ProfileQuota) RetainAntigravityPools(pools []AntigravityQuotaPool) {
	panic("unported: mj-client/src/quota.rs ProfileQuota::retain_antigravity_pools")
}

// ForAntigravityPool is a display projection of one pool; the original
// report retains all scopes for model selection and quota recovery.
func (q *ProfileQuota) ForAntigravityPool(pool AntigravityQuotaPool) ProfileQuota {
	panic("unported: mj-client/src/quota.rs ProfileQuota::for_antigravity_pool")
}

// WindowLabel is a window's name in text that lists a whole report: an
// Antigravity window also names its pool, as in `Week (Ge)`.
func (q *ProfileQuota) WindowLabel(window *QuotaWindow) string {
	if pool, ok := AntigravityPoolOfWindow(window); ok && q.Harness == core.HarnessKindAntigravity {
		return fmt.Sprintf("%s (%s)", window.Label, pool.Label())
	}
	return window.Label
}

// BillingClass is the billing mode inferred from the daemon's cached report.
func (q *ProfileQuota) BillingClass() QuotaBilling {
	switch {
	case q.Error != nil:
		return QuotaBillingUnknown
	case q.IsUsagePriced():
		return QuotaBillingUsage
	case len(q.Windows) > 0 || q.PaidCredits != nil:
		return QuotaBillingSubscription
	default:
		return QuotaBillingUnknown
	}
}

// ProfileQuotaBillingFor is the billing of a profile on harness given its
// last report, if any. A report that classifies the profile decides;
// otherwise the harness may decide on its own (see HarnessBilling).
func ProfileQuotaBillingFor(harness core.HarnessKind, report *ProfileQuota) QuotaBilling {
	billing := QuotaBillingUnknown
	if report != nil {
		billing = report.BillingClass()
	}
	if billing == QuotaBillingUnknown {
		return HarnessBilling(harness)
	}
	return billing
}

// NextCheckAt is, after a refused check, when the daemon checks again. A
// daemon that predates NextCheckAtEpochSeconds checks again when the hold
// ends.
func (q *ProfileQuota) NextCheckAt() (uint64, bool) {
	if q.RateLimitedUntilEpochSeconds == nil {
		return 0, false
	}
	until := *q.RateLimitedUntilEpochSeconds
	next := until
	if q.NextCheckAtEpochSeconds != nil {
		next = *q.NextCheckAtEpochSeconds
	}
	return max(next, until), true
}

// RateLimitLabel is `quota check refused · next in N min` from a refused
// check until the next one runs. The provider refused the check, not the
// account; the windows beside it are the last good reading.
func (q *ProfileQuota) RateLimitLabel(now uint64) (string, bool) {
	next, ok := q.NextCheckAt()
	if !ok || next <= now {
		return "", false
	}
	remaining := next - now
	minutes := remaining / 60
	if remaining%60 != 0 {
		minutes++
	}
	return fmt.Sprintf("quota check refused · next in %d min", minutes), true
}

// RefusedCheckWithoutReading is the refused-check label when there is no
// good reading behind it, so the label stands in for the reading. With a
// good reading, the reading is shown and the label is a note beside it.
func (q *ProfileQuota) RefusedCheckWithoutReading(now uint64) (string, bool) {
	if q.Error == nil {
		return "", false
	}
	return q.RateLimitLabel(now)
}

func (q *ProfileQuota) WeeklyWindow() *QuotaWindow {
	for i := range q.Windows {
		if q.Windows[i].ModelScope == nil && isWeeklyQuotaWindow(q.Windows[i].Label) {
			return &q.Windows[i]
		}
	}
	return nil
}

func (q *ProfileQuota) FiveHourWindow() *QuotaWindow {
	for i := range q.Windows {
		if q.Windows[i].ModelScope == nil && isShortQuotaWindow(q.Windows[i].Label) {
			return &q.Windows[i]
		}
	}
	return nil
}

// FableWeeklyWindow is Claude's model-specific Fable weekly allowance, when
// the provider reports the canonical seven-day scoped window.
func (q *ProfileQuota) FableWeeklyWindow() *QuotaWindow {
	panic("unported: mj-client/src/quota.rs ProfileQuota::fable_weekly_window")
}

// SummaryWindow is the window that best represents this profile at a
// glance. Prefer a long-running allowance, then the short rolling window,
// then any other provider-defined window. Kimi plans without a weekly or
// monthly pool still have a usable rolling limit and must not read as
// unavailable.
func (q *ProfileQuota) SummaryWindow() *QuotaWindow {
	panic("unported: mj-client/src/quota.rs ProfileQuota::summary_window")
}

// IsUsagePriced is whether the report says the profile is usage-priced: an
// API-billed harness has no subscription window to fill, so it reports the
// API label in place of one rather than inventing a percentage.
func (q *ProfileQuota) IsUsagePriced() bool {
	return q.Error == nil && len(q.Windows) == 0 && q.Extra != nil && *q.Extra == APILabel
}

// IsNotReported is whether the report says the harness never shares its
// quota. OpenCode neither publishes an allowance nor reads one from its
// providers, so the profile reads `n/a` rather than as a failed probe.
func (q *ProfileQuota) IsNotReported() bool {
	return q.Error == nil && len(q.Windows) == 0 && q.Extra != nil && *q.Extra == NotReportedLabel
}

// IsIdleWithoutReading is whether the report says the subscription is idle
// with nothing to read: no window is open, and the provider withholds every
// window until the next request opens one.
func (q *ProfileQuota) IsIdleWithoutReading() bool {
	return q.Error == nil && len(q.Windows) == 0 && q.Extra != nil && *q.Extra == IdleLabel
}

// WindowlessLabel is what a report without windows shows in place of a
// reading: `API` for a usage-priced profile, `n/a` for a harness that never
// shares its quota, `idle` for a subscription whose provider reports nothing
// while idle.
func (q *ProfileQuota) WindowlessLabel() (string, bool) {
	switch {
	case q.IsUsagePriced():
		return APILabel, true
	case q.IsNotReported():
		return NotReportedLabel, true
	case q.IsIdleWithoutReading():
		return IdleLabel, true
	}
	return "", false
}

func (q *ProfileQuota) FiveHourProjectsExhaustion() bool {
	window := q.FiveHourWindow()
	return window != nil && projectsExhaustionBeforeReset(window, q.RefreshedAtEpochSeconds)
}

func (q *ProfileQuota) Compact() string {
	panic("unported: mj-client/src/quota.rs ProfileQuota::compact")
}

func (q *ProfileQuota) ErrorLabel() (string, bool) {
	if q.Error == nil {
		return "", false
	}
	return quotaErrorLabel(*q.Error), true
}

// QuotaBilling is how a profile is billed.
type QuotaBilling int

const (
	QuotaBillingUsage QuotaBilling = iota
	QuotaBillingSubscription
	QuotaBillingUnknown
)

// HarnessBilling is the billing a harness implies when no report classifies
// the profile. OpenCode authenticates with API keys and reports no
// allowance, and dsh-acp talks to DeepSeek's API, so both are billed per
// use. Other harnesses are unknown until a report says.
func HarnessBilling(harness core.HarnessKind) QuotaBilling {
	switch harness {
	case core.HarnessKindOpenCode, core.HarnessKindDsh:
		return QuotaBillingUsage
	}
	return QuotaBillingUnknown
}

func quotaErrorLabel(err string) string {
	panic("unported: mj-client/src/quota.rs quota_error_label")
}

// isWeeklyQuotaWindow is the dashboard's long-window column. A harness
// billed monthly rather than weekly belongs in the same column; the label
// itself names the real period.
func isWeeklyQuotaWindow(label string) bool {
	switch strings.ToLower(label) {
	case "week", "weekly", "7d", "month", "monthly":
		return true
	}
	return false
}

func isShortQuotaWindow(label string) bool {
	switch strings.ToLower(label) {
	case "5h", "5-hour", "5 hour":
		return true
	}
	return false
}

// ProjectsExhaustion is whether this window is on course to run out before
// it resets.
func ProjectsExhaustion(window *QuotaWindow, now uint64) bool {
	return projectsExhaustionBeforeReset(window, now)
}

func projectsExhaustionBeforeReset(window *QuotaWindow, now uint64) bool {
	panic("unported: mj-client/src/quota.rs projects_exhaustion_before_reset")
}

// ResetCountdownStyle is which existing TUI countdown convention applies to
// a quota window. Serialized snake_case; the zero value is the default,
// Long.
type ResetCountdownStyle int

const (
	ResetCountdownStyleLong ResetCountdownStyle = iota
	ResetCountdownStyleFiveHour
)

func (s ResetCountdownStyle) MarshalText() ([]byte, error) {
	switch s {
	case ResetCountdownStyleLong:
		return []byte("long"), nil
	case ResetCountdownStyleFiveHour:
		return []byte("five_hour"), nil
	}
	return nil, fmt.Errorf("unknown reset countdown style %d", s)
}

func (s *ResetCountdownStyle) UnmarshalText(text []byte) error {
	switch string(text) {
	case "long":
		*s = ResetCountdownStyleLong
	case "five_hour":
		*s = ResetCountdownStyleFiveHour
	default:
		return fmt.Errorf("unknown variant `%s`, expected `long` or `five_hour`", text)
	}
	return nil
}

// IsFiveHour is whether this is the provider's short rolling five-hour
// window.
func (w *QuotaWindow) IsFiveHour() bool { return isShortQuotaWindow(w.Label) }

func (w *QuotaWindow) ResetCountdownStyle() ResetCountdownStyle {
	if w.IsFiveHour() {
		return ResetCountdownStyleFiveHour
	}
	return ResetCountdownStyleLong
}

func (w *QuotaWindow) ResetDisplay(now uint64, bankedResets *uint64) string {
	return FormatQuotaReset(now, w.ResetsAtEpochSeconds, w.Resets, w.ResetCountdownStyle(), bankedResets)
}

// BankedResetsForWindow: only the long quota window carries the account's
// banked reset balance.
func (q *ProfileQuota) BankedResetsForWindow(window *QuotaWindow) *uint64 {
	if q.BankedResets == nil {
		return nil
	}
	weekly := q.WeeklyWindow()
	if weekly == nil || weekly.Label != window.Label {
		return nil
	}
	return q.BankedResets
}

// FormatQuotaReset renders a reset time and its optional banked balance. The
// browser equivalent is checked against the same examples in
// testdata/quota/reset_display_cases.json.
func FormatQuotaReset(now uint64, reset *int64, fallback *string, style ResetCountdownStyle, bankedResets *uint64) string {
	panic("unported: mj-client/src/quota.rs format_quota_reset")
}

func QuotaResetCountdown(now uint64, resetAtEpochSeconds int64) string {
	panic("unported: mj-client/src/quota.rs quota_reset_countdown")
}

func FiveHourQuotaResetCountdown(now uint64, resetAtEpochSeconds int64) string {
	panic("unported: mj-client/src/quota.rs five_hour_quota_reset_countdown")
}

// MergeResetWindows fills only missing reset data; fresh usage values always
// win. An Antigravity pool is the same allowance whichever of its scopes a
// reading carries, so a cached reading never returns beside a fresh one.
func MergeResetWindows(windows *[]QuotaWindow, cached []QuotaWindow) bool {
	panic("unported: mj-client/src/quota.rs merge_reset_windows")
}

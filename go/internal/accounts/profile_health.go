// Port of mj-accounts/src/profile_health.rs.

// Temporary profile health learned from provider failures (#1361).
//
// A turn that fails because the provider will not serve this account now
// (rate limit, exhausted plan, provider outage) marks its profile and model
// unhealthy for a backoff, so the next spawn chooses another profile instead
// of failing the same way. A later successful turn clears a mark only when
// it completed after that mark was set. A malformed request, a refused login
// (which `RejectedLogins` owns) and network trouble on our side are not
// marks. Quota reports can extend active quota marks to the reported reset.

package accounts

import (
	"fmt"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	// healthBaseBackoff is the first backoff, doubled for each further
	// failure that follows soon after the previous mark ended.
	healthBaseBackoff              = 60 * time.Second
	healthMaxBackoff               = 15 * time.Minute
	healthProviderErrorBaseBackoff = 8 * time.Second
	healthProviderErrorMaxBackoff  = 64 * time.Second
	// healthQuietReset: a failure this long after the previous mark ended
	// starts over at the base backoff for the new fault.
	healthQuietReset = 10 * time.Minute
)

// ProfileFault is why a provider will not serve a profile now.
type ProfileFault int

const (
	ProfileFaultRateLimited ProfileFault = iota
	ProfileFaultQuotaExhausted
	ProfileFaultProviderError
)

// ProfileFaultOf is the fault a failed turn's diagnostic shows, if it is
// the profile's and not the request's. Codex reports typed `sessionFailure`
// categories (`limit` for a rate limit, `service` for a provider error);
// other bridges may report an HTTP status instead.
func ProfileFaultOf(diagnostic *core.TurnDiagnostic) (ProfileFault, bool) {
	if diagnostic.IsUsageLimit() {
		return ProfileFaultQuotaExhausted, true
	}
	if diagnostic.Code != nil {
		switch *diagnostic.Code {
		case "limit":
			return ProfileFaultRateLimited, true
		case "service":
			return ProfileFaultProviderError, true
		}
	}
	if diagnostic.HTTPStatus == nil {
		return 0, false
	}
	switch *diagnostic.HTTPStatus {
	case 429:
		return ProfileFaultRateLimited, true
	case 402:
		return ProfileFaultQuotaExhausted, true
	case 500, 502, 503, 504, 529:
		return ProfileFaultProviderError, true
	}
	return 0, false
}

func (f ProfileFault) describe() string {
	switch f {
	case ProfileFaultRateLimited:
		return "rate-limited"
	case ProfileFaultQuotaExhausted:
		return "out of quota"
	}
	return "failing at the provider"
}

// TurnHealth is what a finished turn shows about its profile and model:
// TurnHealthFailed or TurnHealthSucceeded.
type TurnHealth interface{ isTurnHealth() }

type TurnHealthFailed struct {
	Fault      ProfileFault
	Diagnostic core.TurnDiagnostic
}

// TurnHealthSucceeded is a turn that completed at CompletedAt.
type TurnHealthSucceeded struct{ CompletedAt time.Time }

func (TurnHealthFailed) isTurnHealth()    {}
func (TurnHealthSucceeded) isTurnHealth() {}

// Unhealthy is a profile and model that should not take new work before
// Until.
type Unhealthy struct {
	Fault ProfileFault
	Until time.Time
	// Message is the provider's own words for the failure that set the mark.
	Message string
}

// Describe is "rate-limited until 2026-11-18 14:05:00 UTC (exceeded retry
// limit, ...)". The date is always shown: a weekly reset days away must not
// read as a time later today.
func (u *Unhealthy) Describe() string {
	return fmt.Sprintf("%s until %s (%s)", u.Fault.describe(), u.Until.UTC().Format("2006-01-02 15:04:05 UTC"), u.Message)
}

type healthMark struct {
	unhealthy     Unhealthy
	strikes       uint32
	setAt         time.Time
	explicitReset *int64
}

// healthKey is `(profile_id, model)`.
type healthKey struct {
	profileID string
	model     string
}

// ProfileHealth is the daemon's one record of profile health, keyed by
// profile and model. The zero value is empty and ready to use.
type ProfileHealth struct {
	marks        map[healthKey]*healthMark
	quotaWindows map[string][]client.QuotaWindow
}

// LockedProfileHealth is Rust's `Mutex<ProfileHealth>`.
type LockedProfileHealth struct {
	sync.Mutex
	Health ProfileHealth
}

var sharedProfileHealth = sync.OnceValue(func() *LockedProfileHealth { return &LockedProfileHealth{} })

// ProfileHealthShared is the process-wide owner of provider health marks
// (Rust: `profile_health::shared`).
func ProfileHealthShared() *LockedProfileHealth { return sharedProfileHealth() }

// Record records a failed turn of profileID running model. Failures that
// arrive while the profile is already marked do not lengthen the mark:
// several sessions that started before the mark all fail together, and that
// is one event, not many. A failure soon after a mark ended doubles the
// backoff using the schedule for its fault. A provider reset time, when
// given, sets the end. Quota marks also use the latest quota report.
func (h *ProfileHealth) Record(profileID, model string, fault ProfileFault, diagnostic *core.TurnDiagnostic, now time.Time) {
	panic("unported: mj-accounts/src/profile_health.rs ProfileHealth::record")
}

// Observe records what a finished turn of profileID running model shows.
func (h *ProfileHealth) Observe(profileID, model string, turn TurnHealth, now time.Time) {
	switch turn := turn.(type) {
	case TurnHealthFailed:
		h.Record(profileID, model, turn.Fault, &turn.Diagnostic, now)
	case TurnHealthSucceeded:
		h.RecordSuccess(profileID, model, turn.CompletedAt)
	}
}

// RecordSuccess clears a mark, and its strikes, for a successful turn that
// completed after the mark was set. An older success says nothing about the
// failure that set the mark, for example when a session catches up on
// events recorded before it.
func (h *ProfileHealth) RecordSuccess(profileID, model string, completedAt time.Time) {
	key := healthKey{profileID: profileID, model: model}
	if mark, ok := h.marks[key]; ok && completedAt.After(mark.setAt) {
		delete(h.marks, key)
	}
}

// HasMarks is whether any mark still affects selection or the next
// failure's backoff at now. A mark past its quiet reset is the same as none.
func (h *ProfileHealth) HasMarks(now time.Time) bool {
	for _, mark := range h.marks {
		if mark.unhealthy.Until.Add(healthQuietReset).After(now) {
			return true
		}
	}
	return false
}

// ObserveQuota replaces the latest quota windows for a profile and extends
// active quota marks when the report gives a later reset. A report never
// shortens a mark; only a successful turn ends one early.
func (h *ProfileHealth) ObserveQuota(profileID string, windows []client.QuotaWindow, now time.Time) {
	panic("unported: mj-accounts/src/profile_health.rs ProfileHealth::observe_quota")
}

// ForgetQuota forgets a profile's quota windows when the profile is no
// longer enabled.
func (h *ProfileHealth) ForgetQuota(profileID string) {
	delete(h.quotaWindows, profileID)
}

// Unhealthy is the mark on profileID and model that has not yet ended.
func (h *ProfileHealth) Unhealthy(profileID, model string, now time.Time) *Unhealthy {
	mark, ok := h.marks[healthKey{profileID: profileID, model: model}]
	if !ok || !mark.unhealthy.Until.After(now) {
		return nil
	}
	return &mark.unhealthy
}

// profileHealthQuotaReset is when exhausted quota comes back, as
// autocontinue decides it, but from the exhausted windows only. A window
// with quota left says nothing about the limit that failed, so its reset
// must not hold the mark.
func profileHealthQuotaReset(windows []client.QuotaWindow, model string, explicitReset *int64, now time.Time) (time.Time, bool) {
	panic("unported: mj-accounts/src/profile_health.rs quota_reset")
}

// profileHealthProviderReset is the provider's reset time, when it gave one
// in a supported form.
func profileHealthProviderReset(diagnostic *core.TurnDiagnostic, now time.Time) (time.Time, bool) {
	panic("unported: mj-accounts/src/profile_health.rs provider_reset")
}

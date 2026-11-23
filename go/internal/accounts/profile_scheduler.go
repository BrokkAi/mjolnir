// Port of mj-accounts/src/profile_scheduler.rs.

// Shared capacity ranking and weighted assignment for profiles.

package accounts

import (
	"crypto/rand"
	"encoding/binary"
	"fmt"
	"math"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// CandidateQuotaKind is which reading a CandidateQuota holds.
type CandidateQuotaKind int

const (
	// CandidateQuotaUnknown: no report, a failed one, or one with no usable
	// capacity information. The zero value.
	CandidateQuotaUnknown CandidateQuotaKind = iota
	// CandidateQuotaUsagePriced: billed per use, with no allowance to run out
	// of.
	CandidateQuotaUsagePriced
	// CandidateQuotaSubscription: a subscription with the windows below.
	CandidateQuotaSubscription
)

// CandidateQuota is a candidate's last quota reading, scored for the
// requested model when candidates are ranked. Rust's enum with data; the
// subscription fields are set only when Kind is CandidateQuotaSubscription,
// and the zero value is Unknown.
type CandidateQuota struct {
	Kind CandidateQuotaKind
	// MonthlyPriceUsd is the monthly list price as a capacity proxy, when
	// the plan is recognized.
	MonthlyPriceUsd *uint32
	Windows         []client.QuotaWindow
	PaidCredits     *client.PaidCredits
	// RefreshedAtEpochSeconds is when the windows were read, which is when
	// their usage was measured.
	RefreshedAtEpochSeconds uint64
}

// CandidateQuotaFromReport is the candidate quota a report shows.
func CandidateQuotaFromReport(report *client.ProfileQuota) CandidateQuota {
	if report == nil {
		return CandidateQuota{Kind: CandidateQuotaUnknown}
	}
	switch report.BillingClass() {
	case client.QuotaBillingUsage:
		return CandidateQuota{Kind: CandidateQuotaUsagePriced}
	case client.QuotaBillingSubscription:
		return CandidateQuota{
			Kind:                    CandidateQuotaSubscription,
			MonthlyPriceUsd:         report.MonthlyPriceUsd,
			Windows:                 append([]client.QuotaWindow(nil), report.Windows...),
			PaidCredits:             report.PaidCredits,
			RefreshedAtEpochSeconds: report.RefreshedAtEpochSeconds,
		}
	}
	return CandidateQuota{Kind: CandidateQuotaUnknown}
}

// CandidateQuotaFromPublication is the reading selection should trust.
//
// A probe with no error wins. A failed probe falls back to `last_good`
// when that reading is usage-priced, still has subscription windows, or
// records paid credits. quotaScore treats a window whose reset is at or
// before nowSeconds as already reset, so an exhausted window stops blocking
// once its reset passes. When the reading's billing class is still unknown,
// a harness that is billed per use (`dsh`, API-only OpenCode) is
// usage-priced rather than unknown.
func CandidateQuotaFromPublication(publication *QuotaReportPublication, harness core.HarnessKind, nowSeconds int64) CandidateQuota {
	var report *client.ProfileQuota
	if publication != nil {
		report = schedulingReport(publication, nowSeconds)
	}
	quota := CandidateQuotaFromReport(report)
	if quota.Kind == CandidateQuotaUnknown && quota.Billing(harness) == client.QuotaBillingUsage {
		return CandidateQuota{Kind: CandidateQuotaUsagePriced}
	}
	return quota
}

func schedulingReport(publication *QuotaReportPublication, nowSeconds int64) *client.ProfileQuota {
	if publication.Report != nil && publication.Report.Error == nil {
		return publication.Report
	}
	if publication.LastGood != nil && lastGoodSchedulable(publication.LastGood, nowSeconds) {
		return publication.LastGood
	}
	return nil
}

// lastGoodSchedulable: a failed probe may reuse `last_good` when it is
// usage-priced, it still carries subscription windows, or it records paid
// credits. An expired window is still evidence: scoring treats
// `resets_at_epoch_seconds <= now_seconds` as a reset. A windowless
// non-usage reading with no paid credits is not.
func lastGoodSchedulable(report *client.ProfileQuota, nowSeconds int64) bool {
	panic("unported: mj-accounts/src/profile_scheduler.rs last_good_schedulable")
}

// Billing is the candidate's billing: its report decides, then what the
// harness implies (see client.HarnessBilling).
func (q CandidateQuota) Billing(harness core.HarnessKind) client.QuotaBilling {
	switch q.Kind {
	case CandidateQuotaUsagePriced:
		return client.QuotaBillingUsage
	case CandidateQuotaSubscription:
		return client.QuotaBillingSubscription
	}
	return client.HarnessBilling(harness)
}

// SubagentCandidate is a candidate profile with its discovered choices and
// last quota reading.
type SubagentCandidate struct {
	ProfileID string
	Harness   core.HarnessKind
	Choices   core.ProfileConfig
	Quota     CandidateQuota
	// MayConsumeCredits is the profile's "Load balancing may consume
	// credits" option. See Ineligibility.
	MayConsumeCredits bool
	// Unhealthy is the models this profile should not run now, by model id,
	// from provider failures its sessions just had (#1361).
	Unhealthy map[string]Unhealthy
	// CacheIdentity is the quota cache identity for the profile's current
	// login, matching `quota_history.identity` so velocity telemetry never
	// mixes superseded logins.
	CacheIdentity *string
}

// SubagentCandidateRef is Rust's `AsRef<SubagentCandidate>`: a value that
// carries a subagent candidate.
type SubagentCandidateRef interface {
	AsSubagentCandidate() *SubagentCandidate
}

func (c *SubagentCandidate) AsSubagentCandidate() *SubagentCandidate { return c }

// Placement is which spawn the ranking serves.
type Placement int

const (
	PlacementTopLevel Placement = iota
	PlacementOrchestrator
	PlacementChild
)

type quotaRankKind int

const (
	quotaRankAvailable quotaRankKind = iota
	quotaRankUnknown
	quotaRankPaid
	quotaRankUsagePriced
	quotaRankExhausted
)

// quotaRank is Rust's QuotaRank: value is the weight of Available and Paid.
type quotaRank struct {
	kind  quotaRankKind
	value float64
}

func quotaRankOf(candidate *SubagentCandidate, model string, nowSeconds int64) quotaRank {
	panic("unported: mj-accounts/src/profile_scheduler.rs quota_rank")
}

func paidWeight(credits *client.PaidCredits) (float64, bool) {
	panic("unported: mj-accounts/src/profile_scheduler.rs paid_weight")
}

// Ineligible is why load balancing must not offer a candidate for a model.
type Ineligible int

const (
	// IneligibleOutOfQuota: the subscription is out of quota for the model
	// and has no paid credits.
	IneligibleOutOfQuota Ineligible = iota
	// IneligibleCreditsNotAllowed: the subscription is out of quota; only
	// paid credits remain, and the profile's option forbids load balancing
	// from spending them.
	IneligibleCreditsNotAllowed
)

// Describe is "profile codex2 is out of subscription quota for model ..."
// with what the user can do about it.
func (i Ineligible) Describe(profileID, model string) string {
	if i == IneligibleOutOfQuota {
		return fmt.Sprintf("profile %s is out of subscription quota for model %s", profileID, core.RustDebugString(model))
	}
	return fmt.Sprintf("profile %s is out of subscription quota for model %s; "+
		"enabling \"Load balancing may consume credits\" on the profile would let it "+
		"use its paid credits", profileID, core.RustDebugString(model))
}

// Ineligibility is the one decision whether load balancing may offer a
// candidate. A subscription out of quota for the model is excluded; one with
// only paid credits left is excluded unless its profile allows spending
// them. Usage-priced profiles bill per use by nature, and unknown quota is
// not evidence of exhaustion, so both stay eligible. A session pinned to a
// named profile is not load balancing and does not ask.
func Ineligibility(candidate *SubagentCandidate, model string, nowSeconds int64) (Ineligible, bool) {
	rank := quotaRankOf(candidate, model, nowSeconds)
	switch {
	case rank.kind == quotaRankExhausted:
		return IneligibleOutOfQuota, true
	case rank.kind == quotaRankPaid && candidate.Quota.Kind == CandidateQuotaSubscription && !candidate.MayConsumeCredits:
		return IneligibleCreditsNotAllowed, true
	}
	return 0, false
}

// SubagentCandidates is candidate profiles split into those whose choices
// are known and those whose discovery failed, with the reason.
type SubagentCandidates struct {
	Offered []SubagentCandidate
	// Unavailable is (profile id, reason) pairs.
	Unavailable [][2]string
}

// ReserveFloorPct is the reserve floor in percentage points: usable
// remaining fraction `H_w` is `(remaining_pct - RESERVE_FLOOR_PCT).max(0.0)
// / 100.0`.
const ReserveFloorPct float64 = 1.0

// PaceExponentAlpha is the pace exponent locked from the 2026-11-21 20-seed
// offline simulation.
const PaceExponentAlpha float64 = 2.0

// minPaceRemainingSeconds: minimum fraction `f_w` for a window with reset
// ahead (30 minutes / D_w).
const minPaceRemainingSeconds float64 = 1800.0

// ScheduledCandidate is one `(profile, model, effort)` candidate for
// generalized value- and velocity-based load balancing.
type ScheduledCandidate struct {
	ProfileID         string
	Harness           core.HarnessKind
	Model             string
	Effort            *string
	Quota             CandidateQuota
	MayConsumeCredits bool
	CacheIdentity     *string
}

// ScheduledCandidateRef is Rust's `AsRef<ScheduledCandidate>`: a value that
// carries a scheduled candidate.
type ScheduledCandidateRef interface {
	AsScheduledCandidate() *ScheduledCandidate
}

func (c *ScheduledCandidate) AsScheduledCandidate() *ScheduledCandidate { return c }

// SchedulingContext is shared context for a single automatic scheduling
// draw.
type SchedulingContext struct {
	ExecutionPolicy core.ExecutionPolicy
	Placement       Placement
	Category        *string
	RankingAnchor   *string
	Prefer          []string
	Telemetry       *QuotaTelemetrySnapshot
	PowerRanking    *PowerRankingSnapshot
	NowSeconds      int64
}

// GuardianEligible is the hard raw-target guardian filter: under
// `ConfiguredApprovals`, only harnesses with a native guardian
// (`GuardianReviewer::Harness`: Codex, Claude, Grok, Muse, Antigravity) are
// eligible for automatic scheduling. Under `Unconstrained`, all harnesses
// stay eligible. Explicit `--profile` pins never enter this filter.
func GuardianEligible(harness core.HarnessKind, executionPolicy core.ExecutionPolicy) bool {
	switch executionPolicy {
	case core.ExecutionPolicyUnconstrained:
		return true
	case core.ExecutionPolicyConfiguredApprovals:
		return harness.Guardian() == core.GuardianReviewerHarness
	}
	return false
}

// DrawScheduledCandidates is the generalized pool-first, velocity- and
// value-weighted draw across candidates. Filters out guardian-ineligible,
// exhausted, and disallowed paid-credit candidates, deduplicates to one
// entry per quota pool `(profile_id, binding_window_scope)`, and orders the
// surviving pools by tier, `[quota] prefer`, Efraimidis–Spirakis weighted
// sample, `ranking_anchor`, and `profile_id`.
func DrawScheduledCandidates[T ScheduledCandidateRef](candidates *[]T, ctx *SchedulingContext) error {
	units := make([]float64, 0, len(*candidates))
	for range *candidates {
		var bytes [8]byte
		if _, err := rand.Read(bytes[:]); err != nil {
			return fmt.Errorf("draw a profile: %v", err)
		}
		bits := binary.LittleEndian.Uint64(bytes[:])
		units = append(units, float64((bits>>11)+1)/float64(uint64(1)<<53))
	}
	OrderScheduledCandidatesByDraw(candidates, units, ctx)
	return nil
}

// OrderScheduledCandidatesByDraw is the deterministic counterpart to
// DrawScheduledCandidates taking one uniform `(0, 1]` sample per surviving
// pool in deterministic pre-draw order.
func OrderScheduledCandidatesByDraw[T ScheduledCandidateRef](candidates *[]T, units []float64, ctx *SchedulingContext) {
	panic("unported: mj-accounts/src/profile_scheduler.rs order_scheduled_candidates_by_draw")
}

// RankScheduledCandidates is the deterministic ranking (no random draw):
// orders surviving pools by tier, `[quota] prefer`, descending weight,
// `ranking_anchor`, and `profile_id`.
func RankScheduledCandidates[T ScheduledCandidateRef](candidates *[]T, ctx *SchedulingContext) {
	panic("unported: mj-accounts/src/profile_scheduler.rs rank_scheduled_candidates")
}

type scoredPool[T ScheduledCandidateRef] struct {
	item      T
	tier      uint8
	weight    float64
	preferPos int
	isAnchor  bool
}

type windowEval struct {
	kind         WindowKind
	remainingPct float64
	hW           float64
	uW           float64
	tLeftHours   float64
	modelScope   *string
}

func windowRemainingPct(window *client.QuotaWindow) (float64, bool) {
	if window.Used != nil && window.Limit != nil && *window.Limit > 0 {
		// i64::saturating_sub: plain subtraction clamped at the type's bounds.
		rem := *window.Limit - *window.Used
		if *window.Used < 0 && rem < *window.Limit {
			rem = math.MaxInt64
		} else if *window.Used > 0 && rem > *window.Limit {
			rem = math.MinInt64
		}
		return min(max(100.0*float64(rem)/float64(*window.Limit), 0.0), 100.0), true
	}
	if window.RemainingPercent != nil {
		return float64(*window.RemainingPercent), true
	}
	return 0, false
}

func evaluateApplyingWindows(windows []client.QuotaWindow, model string, nowSeconds int64) []windowEval {
	panic("unported: mj-accounts/src/profile_scheduler.rs evaluate_applying_windows")
}

// bindingWindowScope identifies the quota pool `(profile_id,
// binding_window_scope)` for a candidate. If a scoped applying window
// exists, bind to its scope; otherwise nil (the profile's unscoped shared
// pool).
func bindingWindowScope(candidate *ScheduledCandidate) *string {
	if candidate.Quota.Kind != CandidateQuotaSubscription {
		return nil
	}
	for i := range candidate.Quota.Windows {
		window := &candidate.Quota.Windows[i]
		if window.ModelScope != nil && window.AppliesToModel(candidate.Model) {
			scope := *window.ModelScope
			return &scope
		}
	}
	return nil
}

func prepareScoredPools[T ScheduledCandidateRef](rawCandidates []T, ctx *SchedulingContext) []scoredPool[T] {
	panic("unported: mj-accounts/src/profile_scheduler.rs prepare_scored_pools")
}

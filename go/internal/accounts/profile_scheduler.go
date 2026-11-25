// Port of mj-accounts/src/profile_scheduler.rs.

// Shared capacity ranking and weighted assignment for profiles.

package accounts

import (
	"cmp"
	"crypto/rand"
	"encoding/binary"
	"fmt"
	"log/slog"
	"math"
	"slices"
	"strings"

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
	if report.BillingClass() == client.QuotaBillingUsage || report.PaidCredits != nil {
		return true
	}
	refreshedAt := int64(math.MaxInt64)
	if report.RefreshedAtEpochSeconds <= math.MaxInt64 {
		refreshedAt = int64(report.RefreshedAtEpochSeconds)
	}
	age := telemetrySaturatingSub(nowSeconds, refreshedAt)
	for index := range report.Windows {
		window := &report.Windows[index]
		hasReading := window.RemainingPercent != nil || (window.Used != nil && window.Limit != nil)
		if window.ResetsAtEpochSeconds != nil {
			if hasReading || *window.ResetsAtEpochSeconds <= nowSeconds {
				return true
			}
			continue
		}
		var maxAge int64
		switch {
		case window.DurationSeconds != nil:
			maxAge = *window.DurationSeconds
		case window.IsFiveHour():
			maxAge = 5 * 3600
		default:
			maxAge = 7 * 86_400
		}
		if hasReading && age < maxAge {
			return true
		}
	}
	return false
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
	quota := &candidate.Quota
	switch quota.Kind {
	case CandidateQuotaUnknown:
		return quotaRank{kind: quotaRankUnknown}
	case CandidateQuotaUsagePriced:
		return quotaRank{kind: quotaRankUsagePriced}
	}
	score, scored := QuotaScore(quota.Windows, model, nowSeconds)
	if scored && score > 0 {
		price := 1.0
		if quota.MonthlyPriceUsd != nil && *quota.MonthlyPriceUsd > 0 {
			price = float64(*quota.MonthlyPriceUsd) / 100.0
		}
		return quotaRank{kind: quotaRankAvailable, value: score * price}
	}
	if weight, ok := paidWeight(quota.PaidCredits); ok {
		return quotaRank{kind: quotaRankPaid, value: weight}
	}
	if scored || (len(quota.Windows) == 0 && quota.PaidCredits != nil) {
		return quotaRank{kind: quotaRankExhausted}
	}
	return quotaRank{kind: quotaRankUnknown}
}

func paidWeight(credits *client.PaidCredits) (float64, bool) {
	if credits == nil {
		return 0, false
	}
	switch credits.Kind {
	case client.PaidCreditsLimited:
		balance, ok := jvF64(credits.Balance, true)
		if !ok || math.IsNaN(balance) || math.IsInf(balance, 0) || balance <= 0 {
			return 0, false
		}
		return balance, true
	}
	// No finite balance to compare: give known usable accounts equal
	// weight.
	return 1.0, true
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
	scored := prepareScoredPools(*candidates, ctx)
	type keyedPool struct {
		key  float64
		pool scoredPool[T]
	}
	keyed := make([]keyedPool, 0, len(scored))
	for index, pool := range scored {
		unit := 0.5
		if index < len(units) {
			unit = units[index]
		}
		unit = schedulerClamp(unit, schedulerMinPositive, 1.0)
		key := math.Inf(-1)
		switch pool.tier {
		case 0, 1, 3, 4, 5:
			if pool.weight > 0 {
				key = math.Log(unit) / pool.weight
			}
		}
		keyed = append(keyed, keyedPool{key: key, pool: pool})
	}
	slices.SortStableFunc(keyed, func(left, right keyedPool) int {
		return cmp.Or(
			cmp.Compare(left.pool.tier, right.pool.tier),
			cmp.Compare(left.pool.preferPos, right.pool.preferPos),
			schedulerTotalCmp(right.key, left.key),
			telemetryCompareBool(right.pool.isAnchor, left.pool.isAnchor),
			strings.Compare(left.pool.item.AsScheduledCandidate().ProfileID, right.pool.item.AsScheduledCandidate().ProfileID),
		)
	})
	ordered := make([]T, 0, len(keyed))
	for _, entry := range keyed {
		ordered = append(ordered, entry.pool.item)
	}
	*candidates = ordered
}

// RankScheduledCandidates is the deterministic ranking (no random draw):
// orders surviving pools by tier, `[quota] prefer`, descending weight,
// `ranking_anchor`, and `profile_id`.
func RankScheduledCandidates[T ScheduledCandidateRef](candidates *[]T, ctx *SchedulingContext) {
	scored := prepareScoredPools(*candidates, ctx)
	slices.SortStableFunc(scored, compareScoredPools[T])
	ordered := make([]T, 0, len(scored))
	for _, pool := range scored {
		ordered = append(ordered, pool.item)
	}
	*candidates = ordered
}

// compareScoredPools orders pools by tier, `[quota] prefer`, descending
// weight, `ranking_anchor`, and `profile_id`.
func compareScoredPools[T ScheduledCandidateRef](left, right scoredPool[T]) int {
	return cmp.Or(
		cmp.Compare(left.tier, right.tier),
		cmp.Compare(left.preferPos, right.preferPos),
		schedulerTotalCmp(right.weight, left.weight),
		telemetryCompareBool(right.isAnchor, left.isAnchor),
		strings.Compare(left.item.AsScheduledCandidate().ProfileID, right.item.AsScheduledCandidate().ProfileID),
	)
}

// schedulerMinPositive is Rust's `f64::MIN_POSITIVE`, the smallest positive
// normal value.
const schedulerMinPositive = 0x1p-1022

// schedulerTotalCmp is Rust's `f64::total_cmp`.
func schedulerTotalCmp(left, right float64) int {
	key := func(value float64) int64 {
		bits := int64(math.Float64bits(value))
		return bits ^ int64(uint64(bits>>63)>>1)
	}
	return cmp.Compare(key(left), key(right))
}

// schedulerClamp is Rust's `f64::clamp`, which panics when min > max.
func schedulerClamp(value, low, high float64) float64 {
	if !(low <= high) {
		panic(fmt.Sprintf("min > max, or either was NaN. min = %v, max = %v", low, high))
	}
	if value < low {
		value = low
	}
	if value > high {
		value = high
	}
	return value
}

// schedulerMax is Rust's `f64::max`: a NaN argument yields the other.
func schedulerMax(left, right float64) float64 {
	switch {
	case math.IsNaN(left):
		return right
	case math.IsNaN(right):
		return left
	}
	return math.Max(left, right)
}

// schedulerMin is Rust's `f64::min`: a NaN argument yields the other.
func schedulerMin(left, right float64) float64 {
	switch {
	case math.IsNaN(left):
		return right
	case math.IsNaN(right):
		return left
	}
	return math.Min(left, right)
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
	evals := []windowEval{}
	for index := range windows {
		window := &windows[index]
		if !WindowApplies(window, model) {
			continue
		}
		kind := WindowKindFromWindow(window)
		if window.ResetsAtEpochSeconds != nil && *window.ResetsAtEpochSeconds <= nowSeconds {
			evals = append(evals, windowEval{
				kind:         kind,
				remainingPct: 100.0,
				hW:           (100.0 - ReserveFloorPct) / 100.0,
				uW:           1.0,
				tLeftHours:   0.0,
				modelScope:   cloneOptionalString(window.ModelScope),
			})
			continue
		}
		remainingPct, ok := windowRemainingPct(window)
		if !ok {
			continue
		}
		hW := schedulerMax(remainingPct-ReserveFloorPct, 0.0) / 100.0
		uW, tLeftHours := hW/0.5, 0.0
		if window.ResetsAtEpochSeconds != nil {
			tLeft := float64(*window.ResetsAtEpochSeconds - nowSeconds)
			var duration int64
			switch {
			case window.DurationSeconds != nil && *window.DurationSeconds > 0:
				duration = *window.DurationSeconds
			case kind == WindowKindShort5h:
				duration = 5 * 3600
			default:
				duration = 7 * 86_400
			}
			dW := float64(duration)
			fW := schedulerClamp(tLeft/dW, minPaceRemainingSeconds/dW, 1.0)
			uW, tLeftHours = hW/fW, tLeft/3600.0
		}
		evals = append(evals, windowEval{
			kind:         kind,
			remainingPct: remainingPct,
			hW:           hW,
			uW:           uW,
			tLeftHours:   tLeftHours,
			modelScope:   cloneOptionalString(window.ModelScope),
		})
	}
	return evals
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
		if window.ModelScope != nil && WindowApplies(window, candidate.Model) {
			scope := *window.ModelScope
			return &scope
		}
	}
	return nil
}

func prepareScoredPools[T ScheduledCandidateRef](rawCandidates []T, ctx *SchedulingContext) []scoredPool[T] {
	// tasksPer200 is the lookup's rate, preferring one column and falling
	// back to the other.
	tasksPer200 := func(lookup *PowerRankingLookup, preferAPI bool) *float64 {
		if lookup == nil {
			return nil
		}
		first, second := lookup.TasksPer200Sub, lookup.TasksPer200API
		if preferAPI {
			first, second = second, first
		}
		if first != nil {
			return first
		}
		return second
	}
	usable := func(value *float64) (float64, bool) {
		if value == nil || math.IsNaN(*value) || math.IsInf(*value, 0) || *value <= 0 {
			return 0, false
		}
		return *value, true
	}

	// 1. Hard guardian filter + pool-first deduplication.
	type poolKey struct {
		profileID string
		scope     telemetryScope
	}
	type poolEntry struct {
		tasksPer200 float64
		item        T
	}
	pools := map[poolKey]poolEntry{}
	for _, item := range rawCandidates {
		candidate := item.AsScheduledCandidate()
		if !GuardianEligible(candidate.Harness, ctx.ExecutionPolicy) {
			continue
		}
		if ctx.Placement == PlacementOrchestrator && !candidate.Harness.SupportsDelegationTools() {
			continue
		}
		lookup := ctx.PowerRanking.Lookup(candidate.Model, candidate.Effort, candidate.Harness)
		entryTasksPer200 := 1.0
		if rate := tasksPer200(lookup, candidate.Quota.Kind == CandidateQuotaUsagePriced); rate != nil {
			entryTasksPer200 = *rate
		}
		key := poolKey{profileID: candidate.ProfileID, scope: telemetryScopeOf(bindingWindowScope(candidate))}
		existing, occupied := pools[key]
		if !occupied {
			pools[key] = poolEntry{tasksPer200: entryTasksPer200, item: item}
			continue
		}
		existingCandidate := existing.item.AsScheduledCandidate()
		replace := entryTasksPer200 > existing.tasksPer200 ||
			(entryTasksPer200 == existing.tasksPer200 &&
				cmp.Or(
					strings.Compare(candidate.Model, existingCandidate.Model),
					schedulerCompareOptional(candidate.Effort, existingCandidate.Effort),
				) < 0)
		if replace {
			pools[key] = poolEntry{tasksPer200: entryTasksPer200, item: item}
		}
	}
	keys := make([]poolKey, 0, len(pools))
	for key := range pools {
		keys = append(keys, key)
	}
	slices.SortFunc(keys, func(left, right poolKey) int {
		return cmp.Or(
			strings.Compare(left.profileID, right.profileID),
			telemetryCompareBool(left.scope.present, right.scope.present),
			strings.Compare(left.scope.scope, right.scope.scope),
		)
	})
	poolItems := make([]T, 0, len(keys))
	for _, key := range keys {
		poolItems = append(poolItems, pools[key].item)
	}

	// 2. Compute geometric mean of `tasks_per_200_sub * (P_eff / 200.0)`
	//    across priced subscription peers in this draw, for unpriced peers.
	logSum, pricedCount := 0.0, 0
	for _, item := range poolItems {
		candidate := item.AsScheduledCandidate()
		if candidate.Quota.Kind != CandidateQuotaSubscription {
			continue
		}
		lookup := ctx.PowerRanking.Lookup(candidate.Model, candidate.Effort, candidate.Harness)
		pEff, pricedOK := 0.0, false
		if price := candidate.Quota.MonthlyPriceUsd; price != nil && *price > 0 {
			pEff, pricedOK = float64(*price), true
		} else if lookup != nil {
			pEff, pricedOK = usable(lookup.PlanUsdPerMonth)
		}
		rate, rateOK := usable(tasksPer200(lookup, false))
		if pricedOK && rateOK {
			if capacity := rate * (pEff / 200.0); capacity > 0 {
				logSum += math.Log(capacity)
				pricedCount++
			}
		}
	}
	peerGeomMeanCapacity := 1.0
	if pricedCount > 0 {
		peerGeomMeanCapacity = math.Exp(logSum / float64(pricedCount))
	}

	category := "primary"
	if ctx.Category != nil {
		category = *ctx.Category
	}
	catStats := ctx.Telemetry.CategoryStatsFor(category)

	// 3. Score each pool, filtering out Exhausted (tier 6) and disallowed
	//    Paid.
	scored := make([]scoredPool[T], 0, len(poolItems))
	for _, item := range poolItems {
		candidate := item.AsScheduledCandidate()
		lookup := ctx.PowerRanking.Lookup(candidate.Model, candidate.Effort, candidate.Harness)
		preferPos := slices.Index(ctx.Prefer, candidate.ProfileID)
		if preferPos < 0 {
			preferPos = math.MaxInt
		}
		isAnchor := ctx.RankingAnchor != nil && *ctx.RankingAnchor == candidate.ProfileID
		subscriptionRate := func() float64 {
			if rate, ok := usable(tasksPer200(lookup, false)); ok {
				return rate
			}
			return 1.0
		}

		var (
			tier   uint8
			weight float64
		)
		quota := &candidate.Quota
		switch quota.Kind {
		case CandidateQuotaUnknown:
			tier, weight = 2, 1.0
		case CandidateQuotaUsagePriced:
			rate, ok := usable(tasksPer200(lookup, true))
			if !ok {
				rate = 1.0
			}
			tier, weight = 5, rate
		default:
			evals := evaluateApplyingWindows(quota.Windows, candidate.Model, ctx.NowSeconds)
			exhausted := false
			for _, eval := range evals {
				if eval.remainingPct <= 0 {
					exhausted = true
				}
			}
			switch {
			case len(evals) == 0:
				if creditWeight, ok := paidWeight(quota.PaidCredits); ok {
					if !candidate.MayConsumeCredits {
						continue
					}
					tier, weight = 4, subscriptionRate()*creditWeight
				} else if len(quota.Windows) == 0 && quota.PaidCredits != nil {
					// Exhausted paid-only subscription with 0 credits.
					continue
				} else {
					tier, weight = 2, 1.0
				}
			case exhausted:
				// Exhausted subscription: fall back to Paid credits if
				// allowed, else drop (tier 6).
				creditWeight, ok := paidWeight(quota.PaidCredits)
				if !ok || !candidate.MayConsumeCredits {
					continue
				}
				tier, weight = 4, subscriptionRate()*creditWeight
			default:
				tier, weight = scoreSubscriptionPool(ctx, candidate, lookup, evals, peerGeomMeanCapacity, catStats)
			}
		}

		scored = append(scored, scoredPool[T]{
			item:      item,
			tier:      tier,
			weight:    weight,
			preferPos: preferPos,
			isAnchor:  isAnchor,
		})
	}

	// Deterministic pre-draw order so `units[i]` binds to a stable pool
	// order.
	slices.SortStableFunc(scored, compareScoredPools[T])
	return scored
}

// scoreSubscriptionPool is the tier and weight of a subscription pool with
// quota left on every applying window.
func scoreSubscriptionPool(ctx *SchedulingContext, candidate *ScheduledCandidate, lookup *PowerRankingLookup, evals []windowEval, peerGeomMeanCapacity float64, catStats CategoryBurnStats) (uint8, float64) {
	usable := func(value *float64) *float64 {
		if value == nil || math.IsNaN(*value) || math.IsInf(*value, 0) || *value <= 0 {
			return nil
		}
		return value
	}
	var planPrice, weeklyBudget, rate *float64
	if lookup != nil {
		planPrice = usable(lookup.PlanUsdPerMonth)
		weeklyBudget = usable(lookup.WeeklyBudgetUsd)
		if lookup.TasksPer200Sub != nil {
			rate = usable(lookup.TasksPer200Sub)
		} else {
			rate = usable(lookup.TasksPer200API)
		}
	}
	var inferredPrice *float64
	if price := candidate.Quota.MonthlyPriceUsd; price != nil && *price > 0 {
		value := float64(*price)
		inferredPrice = &value
	}
	pEffOpt := inferredPrice
	if pEffOpt == nil {
		pEffOpt = planPrice
	}

	var baseCapacity, pEffForBudget float64
	switch {
	case pEffOpt != nil && rate != nil:
		if inferredPrice == nil {
			slog.Debug("using power-ranking plan_usd_per_month as P_eff",
				"profile_id", candidate.ProfileID, "model", candidate.Model, "plan_usd_per_month", *pEffOpt)
		}
		baseCapacity, pEffForBudget = *rate*(*pEffOpt/200.0), *pEffOpt
	case pEffOpt != nil:
		baseCapacity, pEffForBudget = peerGeomMeanCapacity*(*pEffOpt/200.0), *pEffOpt
	default:
		slog.Debug("using geometric mean peer capacity for unpriced subscription pool",
			"profile_id", candidate.ProfileID, "model", candidate.Model, "assumed_capacity", peerGeomMeanCapacity)
		pEffForBudget = 200.0
		if planPrice != nil {
			pEffForBudget = *planPrice
		}
		baseCapacity = peerGeomMeanCapacity
	}

	var uLong, u5h *float64
	allAboveReserve, fiveHourSafe, weeklySafe := true, true, true
	for _, eval := range evals {
		if eval.hW <= 0 {
			allAboveReserve = false
		}
		switch eval.kind {
		case WindowKindLong:
			value := eval.uW
			if uLong != nil {
				value = schedulerMin(*uLong, eval.uW)
			}
			uLong = &value
		case WindowKindShort5h:
			value := eval.uW
			if u5h != nil {
				value = schedulerMin(*u5h, eval.uW)
			}
			u5h = &value
		}
		if eval.tLeftHours > 0 {
			telemetry := ctx.Telemetry.WindowTelemetry(candidate.ProfileID, candidate.CacheIdentity, eval.kind, eval.modelScope)
			v60m := 0.0
			var secondToLast *int64
			var ratio *float64
			if telemetry != nil {
				v60m = telemetry.Trailing60mPctPerHour
				secondToLast = telemetry.SecondToLastCleanTs
				ratio = telemetry.FiveHourToWeeklyRatio
			}
			reserved := ctx.Telemetry.ReservedPct(candidate.ProfileID, eval.kind, secondToLast, weeklyBudget, pEffForBudget, planPrice, ratio)
			newBurn := MeanBurnForWindowPct(catStats.MeanAPIUsd, eval.kind, weeklyBudget, pEffForBudget, planPrice, ratio)
			remEff := eval.remainingPct - ReserveFloorPct - reserved - newBurn
			safe := remEff >= v60m*schedulerMin(catStats.RunwayDurationHours, eval.tLeftHours)
			if !safe {
				switch eval.kind {
				case WindowKindShort5h:
					fiveHourSafe = false
				case WindowKindLong:
					weeklySafe = false
				}
			}
		}
	}

	var weight float64
	switch {
	case uLong != nil && u5h != nil:
		weight = baseCapacity * math.Pow(*uLong, PaceExponentAlpha) * schedulerMin(*u5h, 1.0)
	case uLong != nil:
		weight = baseCapacity * math.Pow(*uLong, PaceExponentAlpha)
	case u5h != nil:
		weight = baseCapacity * math.Pow(*u5h, PaceExponentAlpha)
	default:
		weight = baseCapacity
	}
	weight = schedulerMax(weight, schedulerMinPositive)

	switch {
	case !allAboveReserve || !weeklySafe:
		return 3, weight
	case fiveHourSafe:
		return 0, weight
	}
	return 1, weight
}

// schedulerCompareOptional orders optional strings as Rust's Option does:
// None first.
func schedulerCompareOptional(left, right *string) int {
	switch {
	case left == nil && right == nil:
		return 0
	case left == nil:
		return -1
	case right == nil:
		return 1
	}
	return strings.Compare(*left, *right)
}

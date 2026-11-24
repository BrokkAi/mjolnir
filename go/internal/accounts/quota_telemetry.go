// Port of mj-accounts/src/quota_telemetry.rs.

// Trailing-60-minute quota burn velocity, durable placement reservations,
// and category burn statistics for the unified runway gate.

package accounts

import (
	"cmp"
	"fmt"
	"math"
	"slices"
	"sort"
	"sync"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
)

// resetShiftToleranceSeconds is the tolerance for reset-deadline jitter
// before an interval is treated as a shifted deadline (Codex / Antigravity)
// and dropped from velocity.
const resetShiftToleranceSeconds int64 = 120

// velocityWindowSeconds is the trailing window over which clean
// `quota_history` intervals are integrated.
const velocityWindowSeconds int64 = 60 * 60

// HistoryLookbackSeconds is the lookback window loaded from `quota_history`
// so the first clean reading before the 60-minute boundary is included.
const HistoryLookbackSeconds uint64 = 90 * 60

// singleReadingReservationLookbackSeconds is the default reservation
// lookback when only a single clean reading exists.
const singleReadingReservationLookbackSeconds int64 = 10 * 60

// LocalCategorySwitchoverMinSamples is the minimum completed sessions with
// `category` populated before replacing the measured default burn/duration
// constants with local SQLite observations.
const LocalCategorySwitchoverMinSamples = 200

// LocalCategoryMaxSamples is the maximum recent completed sessions
// inspected per category during the hourly background refresh.
const LocalCategoryMaxSamples = 1000

// WindowKind is a quota window's length class. Short5h orders before Long.
type WindowKind int

const (
	WindowKindShort5h WindowKind = iota
	WindowKindLong
)

func WindowKindFromWindow(window *client.QuotaWindow) WindowKind {
	if window.IsFiveHour() {
		return WindowKindShort5h
	}
	return WindowKindLong
}

// WindowKey identifies one telemetry series. It is comparable so it can key
// a Go map; ModelScope with HasModelScope false is Rust's `None`.
type WindowKey struct {
	ProfileID     string
	CacheIdentity string
	WindowKind    WindowKind
	ModelScope    string
	HasModelScope bool
}

// NewWindowKey builds a key from an optional model scope.
func NewWindowKey(profileID, cacheIdentity string, windowKind WindowKind, modelScope *string) WindowKey {
	key := WindowKey{ProfileID: profileID, CacheIdentity: cacheIdentity, WindowKind: windowKind}
	if modelScope != nil {
		key.ModelScope, key.HasModelScope = *modelScope, true
	}
	return key
}

// Scope is the key's model scope, nil when it has none.
func (k WindowKey) Scope() *string {
	if !k.HasModelScope {
		return nil
	}
	scope := k.ModelScope
	return &scope
}

// Compare orders keys as Rust's derived Ord does (a missing scope first).
func (k WindowKey) Compare(other WindowKey) int {
	return cmp.Or(
		cmp.Compare(k.ProfileID, other.ProfileID),
		cmp.Compare(k.CacheIdentity, other.CacheIdentity),
		cmp.Compare(k.WindowKind, other.WindowKind),
		telemetryCompareBool(k.HasModelScope, other.HasModelScope),
		cmp.Compare(k.ModelScope, other.ModelScope),
	)
}

func telemetryCompareBool(left, right bool) int {
	switch {
	case left == right:
		return 0
	case !left:
		return -1
	}
	return 1
}

type WindowTelemetry struct {
	Trailing60mPctPerHour float64
	SecondToLastCleanTs   *int64
	FiveHourToWeeklyRatio *float64
}

type RecentPlacement struct {
	Category              string
	CreatedAtEpochSeconds int64
}

type CategoryBurnStats struct {
	MeanAPIUsd          float64
	RunwayDurationHours float64
	LocalSampleCount    int
}

// DefaultCategoryBurnStats is the measured category burn defaults from the
// 2026-11-21 live-store empirical analysis (180,417 turns / 50,192
// children; `c4ecd0f90d14c80a4f0d25104e36eebe/report.md`), with conservative
// initial defaults for unmeasured categories (`oracle`, `orchestrator`,
// `primary`). Replaced per category once `N >= 200` local completed
// sessions accumulate.
func DefaultCategoryBurnStats(category string) CategoryBurnStats {
	switch category {
	case "straightforward":
		return CategoryBurnStats{MeanAPIUsd: 0.12, RunwayDurationHours: 71.0 / 60.0}
	case "subtle":
		return CategoryBurnStats{MeanAPIUsd: 3.47, RunwayDurationHours: 166.0 / 60.0}
	case "reviewer", "brief_writer":
		return CategoryBurnStats{MeanAPIUsd: 0.22, RunwayDurationHours: 6.0 / 60.0}
	case "oracle":
		return CategoryBurnStats{MeanAPIUsd: 0.50, RunwayDurationHours: 15.0 / 60.0}
	case "supervisor", "orchestrator", "primary":
		return CategoryBurnStats{MeanAPIUsd: 5.00, RunwayDurationHours: 180.0 / 60.0}
	}
	return CategoryBurnStats{MeanAPIUsd: 0.50, RunwayDurationHours: 1.0}
}

// MeanBurnWeeklyPct converts `mean_api_usd` into weekly percentage points
// using `weekly_budget_usd` from PowerRankingSnapshot scaled by
// `p_eff / plan_usd_per_month`.
func MeanBurnWeeklyPct(meanAPIUsd float64, weeklyBudgetUsd *float64, pEff float64, planUsdPerMonth *float64) float64 {
	usable := func(value *float64) bool {
		return value != nil && !math.IsInf(*value, 0) && !math.IsNaN(*value) && *value > 0
	}
	if !usable(weeklyBudgetUsd) {
		return 0
	}
	scale := 1.0
	if usable(planUsdPerMonth) && usable(&pEff) {
		scale = pEff / *planUsdPerMonth
	}
	effectiveBudget := *weeklyBudgetUsd * scale
	if !usable(&effectiveBudget) {
		return 0
	}
	return max(100*meanAPIUsd/effectiveBudget, 0)
}

// MeanBurnForWindowPct converts a weekly percentage burn into a 5-hour
// percentage burn when the profile has an observed
// `five_hour_to_weekly_ratio`. When no paired ratio is observed, returns 0
// so the 5h runway gate relies on `v_5h * min(d_category, t_5h_left)`.
func MeanBurnForWindowPct(meanAPIUsd float64, windowKind WindowKind, weeklyBudgetUsd *float64, pEff float64, planUsdPerMonth *float64, fiveHourToWeeklyRatio *float64) float64 {
	weeklyPct := MeanBurnWeeklyPct(meanAPIUsd, weeklyBudgetUsd, pEff, planUsdPerMonth)
	if windowKind == WindowKindLong {
		return weeklyPct
	}
	if ratio := fiveHourToWeeklyRatio; ratio != nil && !math.IsInf(*ratio, 0) && !math.IsNaN(*ratio) && *ratio > 0 {
		return weeklyPct * *ratio
	}
	return 0
}

type QuotaTelemetrySnapshot struct {
	Windows          map[WindowKey]WindowTelemetry
	RecentPlacements map[string][]RecentPlacement
	CategoryStats    map[string]CategoryBurnStats
}

// CategoryStatsFor is the category's local statistics, else its defaults
// (Rust: `QuotaTelemetrySnapshot::category_stats`, renamed because Go
// cannot give a method its field's name).
func (s *QuotaTelemetrySnapshot) CategoryStatsFor(category string) CategoryBurnStats {
	if stats, ok := s.CategoryStats[category]; ok {
		return stats
	}
	return DefaultCategoryBurnStats(category)
}

// WindowTelemetry is the telemetry of one window. With a cache identity
// only that exact login's series matches; without one, the first series of
// the profile, kind and scope in key order.
func (s *QuotaTelemetrySnapshot) WindowTelemetry(profileID string, cacheIdentity *string, windowKind WindowKind, modelScope *string) *WindowTelemetry {
	if cacheIdentity != nil {
		// Exact match on `(profile_id, cache_identity)`: never mix history
		// from a superseded login identity.
		telemetry, ok := s.Windows[NewWindowKey(profileID, *cacheIdentity, windowKind, modelScope)]
		if !ok {
			return nil
		}
		return &telemetry
	}
	// Fallback match by `(profile_id, window_kind, model_scope)` only when
	// callers do not carry `cache_identity`: the first in key order.
	var (
		best  WindowKey
		found bool
	)
	for key := range s.Windows {
		if key.ProfileID != profileID || key.WindowKind != windowKind || key.HasModelScope != (modelScope != nil) ||
			(modelScope != nil && key.ModelScope != *modelScope) {
			continue
		}
		if !found || key.Compare(best) < 0 {
			best, found = key, true
		}
	}
	if !found {
		return nil
	}
	telemetry := s.Windows[best]
	return &telemetry
}

// ReservedPct sums `mean_burn_pct(placement.category, window_kind)` over
// placements on profileID created strictly after secondToLastCleanTs (or
// all recent placements if secondToLastCleanTs is nil).
func (s *QuotaTelemetrySnapshot) ReservedPct(profileID string, windowKind WindowKind, secondToLastCleanTs *int64, weeklyBudgetUsd *float64, pEff float64, planUsdPerMonth *float64, fiveHourToWeeklyRatio *float64) float64 {
	placements, ok := s.RecentPlacements[profileID]
	if !ok {
		return 0
	}
	sum := 0.0
	for _, placement := range placements {
		if secondToLastCleanTs != nil && placement.CreatedAtEpochSeconds <= *secondToLastCleanTs {
			continue
		}
		stats := s.CategoryStatsFor(placement.Category)
		sum += MeanBurnForWindowPct(stats.MeanAPIUsd, windowKind, weeklyBudgetUsd, pEff, planUsdPerMonth, fiveHourToWeeklyRatio)
	}
	return sum
}

// windowUsedPct extracts usage percentage in `[0.0, 100.0]` from a
// QuotaWindow: `100.0 * used / limit` when both `used` and `limit > 0`, else
// `100.0 - remaining_percent`.
func windowUsedPct(window *client.QuotaWindow) (float64, bool) {
	if window.Used != nil && window.Limit != nil && *window.Limit > 0 {
		return min(max(100*float64(*window.Used)/float64(*window.Limit), 0), 100), true
	}
	if window.RemainingPercent != nil {
		return min(max(100-float64(*window.RemainingPercent), 0), 100), true
	}
	return 0, false
}

type samplePoint struct {
	ts       int64
	usedPct  float64
	resetsAt *int64
}

// ComputeWindowTelemetry computes WindowTelemetry entries for one
// `(profile_id, cache_identity)` from raw `quota_history` rows (in any
// order).
func ComputeWindowTelemetry(profileID, cacheIdentity string, history []client.ProfileQuota, nowSeconds int64) map[WindowKey]WindowTelemetry {
	clean := []*client.ProfileQuota{}
	for index := range history {
		if history[index].Error == nil && len(history[index].Windows) > 0 {
			clean = append(clean, &history[index])
		}
	}
	sort.SliceStable(clean, func(left, right int) bool {
		return clean[left].RefreshedAtEpochSeconds < clean[right].RefreshedAtEpochSeconds
	})
	timestamp := func(row *client.ProfileQuota) int64 {
		if row.RefreshedAtEpochSeconds > math.MaxInt64 {
			return math.MaxInt64
		}
		return int64(row.RefreshedAtEpochSeconds)
	}

	cutoff := telemetrySaturatingSub(nowSeconds, velocityWindowSeconds)

	// Group clean samples by (window_kind, model_scope).
	type seriesKey struct {
		kind  WindowKind
		scope telemetryScope
	}
	series := map[seriesKey][]samplePoint{}
	seriesOrder := []seriesKey{}
	for _, row := range clean {
		ts := timestamp(row)
		for index := range row.Windows {
			window := &row.Windows[index]
			usedPct, ok := windowUsedPct(window)
			if !ok {
				continue
			}
			key := seriesKey{kind: WindowKindFromWindow(window), scope: telemetryScopeOf(window.ModelScope)}
			if _, seen := series[key]; !seen {
				seriesOrder = append(seriesOrder, key)
			}
			series[key] = append(series[key], samplePoint{ts: ts, usedPct: usedPct, resetsAt: window.ResetsAtEpochSeconds})
		}
	}

	// Compute paired 5h-to-weekly ratio per model_scope (and unscoped
	// fallback) across consecutive clean rows that carry both a valid 5h and
	// Long delta.
	ratioByScope := map[telemetryScope][2]float64{}
	for pairIndex := 0; pairIndex+1 < len(clean); pairIndex++ {
		prev, cur := clean[pairIndex], clean[pairIndex+1]
		prevTs, curTs := timestamp(prev), timestamp(cur)
		if curTs <= prevTs {
			continue
		}
		// Match windows by (kind, model_scope) between prev and cur.
		shortDeltas := map[telemetryScope]float64{}
		longDeltas := map[telemetryScope]float64{}
		for curIndex := range cur.Windows {
			curWindow := &cur.Windows[curIndex]
			curUsed, ok := windowUsedPct(curWindow)
			if !ok {
				continue
			}
			kind := WindowKindFromWindow(curWindow)
			curScope := telemetryScopeOf(curWindow.ModelScope)
			var prevWindow *client.QuotaWindow
			for prevIndex := range prev.Windows {
				candidate := &prev.Windows[prevIndex]
				if WindowKindFromWindow(candidate) == kind && telemetryScopeOf(candidate.ModelScope) == curScope {
					prevWindow = candidate
					break
				}
			}
			if prevWindow == nil {
				continue
			}
			prevUsed, ok := windowUsedPct(prevWindow)
			if !ok {
				continue
			}
			if isCorruptedInterval(prevTs, curTs, prevUsed, curUsed, prevWindow.ResetsAtEpochSeconds, curWindow.ResetsAtEpochSeconds) {
				continue
			}
			delta := curUsed - prevUsed
			if delta > 0 {
				switch kind {
				case WindowKindShort5h:
					shortDeltas[curScope] = delta
				case WindowKindLong:
					longDeltas[curScope] = delta
				}
			}
		}
		for scope, delta5h := range shortDeltas {
			deltaLong, ok := longDeltas[scope]
			if !ok {
				deltaLong, ok = longDeltas[telemetryScope{}]
			}
			if ok && deltaLong > 0 {
				entry := ratioByScope[scope]
				entry[0] += delta5h
				entry[1] += deltaLong
				ratioByScope[scope] = entry
			}
		}
	}

	out := map[WindowKey]WindowTelemetry{}
	for _, key := range seriesOrder {
		points := series[key]
		var secondToLastCleanTs *int64
		switch len(points) {
		case 0:
		case 1:
			value := telemetrySaturatingSub(points[0].ts, singleReadingReservationLookbackSeconds)
			secondToLastCleanTs = &value
		default:
			value := points[len(points)-2].ts
			secondToLastCleanTs = &value
		}

		sumDeltaUsed, sumDeltaHours := 0.0, 0.0
		for index := 0; index+1 < len(points); index++ {
			prev, cur := &points[index], &points[index+1]
			// Only integrate intervals whose end lies within the trailing 60
			// minutes.
			if cur.ts < cutoff {
				continue
			}
			if isCorruptedInterval(prev.ts, cur.ts, prev.usedPct, cur.usedPct, prev.resetsAt, cur.resetsAt) {
				continue
			}
			sumDeltaUsed += cur.usedPct - prev.usedPct
			sumDeltaHours += float64(cur.ts-prev.ts) / 3600.0
		}
		trailing := 0.0
		if sumDeltaHours > 0 {
			trailing = max(sumDeltaUsed/sumDeltaHours, 0)
		}

		var fiveHourToWeeklyRatio *float64
		ratio, ok := ratioByScope[key.scope]
		if !ok {
			ratio, ok = ratioByScope[telemetryScope{}]
		}
		if ok && ratio[1] > 0 && ratio[0] > 0 {
			value := ratio[0] / ratio[1]
			fiveHourToWeeklyRatio = &value
		}

		out[NewWindowKey(profileID, cacheIdentity, key.kind, key.scope.pointer())] = WindowTelemetry{
			Trailing60mPctPerHour: trailing,
			SecondToLastCleanTs:   secondToLastCleanTs,
			FiveHourToWeeklyRatio: fiveHourToWeeklyRatio,
		}
	}
	return out
}

// telemetryScope is an optional model scope usable as a map key; the zero
// value is Rust's `None`.
type telemetryScope struct {
	scope   string
	present bool
}

func telemetryScopeOf(scope *string) telemetryScope {
	if scope == nil {
		return telemetryScope{}
	}
	return telemetryScope{scope: *scope, present: true}
}

func (s telemetryScope) pointer() *string {
	if !s.present {
		return nil
	}
	scope := s.scope
	return &scope
}

// telemetrySaturatingSub is Rust's `i64::saturating_sub`.
func telemetrySaturatingSub(a, b int64) int64 {
	difference := a - b
	if b > 0 && difference > a {
		return math.MinInt64
	}
	if b < 0 && difference < a {
		return math.MaxInt64
	}
	return difference
}

func isCorruptedInterval(prevTs, curTs int64, prevUsed, curUsed float64, prevReset, curReset *int64) bool {
	if curTs <= prevTs || curUsed < prevUsed {
		return true
	}
	if prevReset != nil && curReset != nil {
		shift := *curReset - *prevReset
		if shift > resetShiftToleranceSeconds || shift < -resetShiftToleranceSeconds {
			return true
		}
	}
	return false
}

// profilePlacement is a placement on a profile: Rust's `(String,
// RecentPlacement)`.
type profilePlacement struct {
	profileID string
	placement RecentPlacement
}

// committedPlacementKey is `(session_id, profile_id)`.
type committedPlacementKey struct {
	sessionID string
	profileID string
}

type quotaTelemetryStoreInner struct {
	windows           map[WindowKey]WindowTelemetry
	durablePlacements map[string][]RecentPlacement
	categoryStats     map[string]CategoryBurnStats
	nextReservationID uint64
	inFlight          map[uint64]profilePlacement
	committedPending  map[committedPlacementKey]profilePlacement
}

func (i *quotaTelemetryStoreInner) buildSnapshot() QuotaTelemetrySnapshot {
	recentPlacements := make(map[string][]RecentPlacement, len(i.durablePlacements))
	for profileID, placements := range i.durablePlacements {
		recentPlacements[profileID] = slices.Clone(placements)
	}
	// Rust's BTreeMaps add the pending and in-flight placements in key
	// order.
	pendingKeys := make([]committedPlacementKey, 0, len(i.committedPending))
	for key := range i.committedPending {
		pendingKeys = append(pendingKeys, key)
	}
	slices.SortFunc(pendingKeys, func(left, right committedPlacementKey) int {
		return cmp.Or(cmp.Compare(left.sessionID, right.sessionID), cmp.Compare(left.profileID, right.profileID))
	})
	for _, key := range pendingKeys {
		entry := i.committedPending[key]
		recentPlacements[entry.profileID] = append(recentPlacements[entry.profileID], entry.placement)
	}
	inFlightKeys := make([]uint64, 0, len(i.inFlight))
	for key := range i.inFlight {
		inFlightKeys = append(inFlightKeys, key)
	}
	slices.Sort(inFlightKeys)
	for _, key := range inFlightKeys {
		entry := i.inFlight[key]
		recentPlacements[entry.profileID] = append(recentPlacements[entry.profileID], entry.placement)
	}
	windows := make(map[WindowKey]WindowTelemetry, len(i.windows))
	for key, value := range i.windows {
		windows[key] = value
	}
	categoryStats := make(map[string]CategoryBurnStats, len(i.categoryStats))
	for key, value := range i.categoryStats {
		categoryStats[key] = value
	}
	return QuotaTelemetrySnapshot{
		Windows:          windows,
		RecentPlacements: recentPlacements,
		CategoryStats:    categoryStats,
	}
}

// PlacementReservation is the handle for a tentative in-flight placement
// reservation created atomically during DrawAndReserve.
//
// Releasing it without calling Commit releases the reservation immediately
// so failed spawns/switches never leak reserved burn. Rust releases on
// drop; Go callers `defer reservation.Release()`.
type PlacementReservation struct {
	store         *QuotaTelemetryStore
	reservationID uint64
	committed     atomic.Bool
}

// String is Rust's Debug: the reservation id and whether it was committed.
func (r *PlacementReservation) String() string {
	return fmt.Sprintf("PlacementReservation { reservation_id: %d, committed: %t }", r.reservationID, r.committed.Load())
}

// Commit promotes this tentative reservation to a committed placement.
// Passing a session id enables exact deduplication on RefreshFromStore.
func (r *PlacementReservation) Commit(sessionID *string) {
	r.committed.Store(true)
	r.store.mu.Lock()
	defer r.store.mu.Unlock()
	entry, ok := r.store.inner.inFlight[r.reservationID]
	if !ok {
		return
	}
	delete(r.store.inner.inFlight, r.reservationID)
	key := committedPlacementKey{profileID: entry.profileID}
	if sessionID != nil {
		key.sessionID = *sessionID
	} else {
		key.sessionID = fmt.Sprintf("__unkeyed-%d", r.reservationID)
	}
	if r.store.inner.committedPending == nil {
		r.store.inner.committedPending = map[committedPlacementKey]profilePlacement{}
	}
	r.store.inner.committedPending[key] = entry
}

// Release drops an uncommitted reservation (Rust's Drop).
func (r *PlacementReservation) Release() {
	if r == nil || r.committed.Load() {
		return
	}
	r.store.mu.Lock()
	defer r.store.mu.Unlock()
	delete(r.store.inner.inFlight, r.reservationID)
}

// QuotaTelemetryStore is the shared handle to the daemon's current
// QuotaTelemetrySnapshot. Share it by pointer.
type QuotaTelemetryStore struct {
	mu    sync.RWMutex
	inner quotaTelemetryStoreInner
}

func NewQuotaTelemetryStore() *QuotaTelemetryStore {
	return &QuotaTelemetryStore{inner: quotaTelemetryStoreInner{
		windows:           map[WindowKey]WindowTelemetry{},
		durablePlacements: map[string][]RecentPlacement{},
		categoryStats:     map[string]CategoryBurnStats{},
		inFlight:          map[uint64]profilePlacement{},
		committedPending:  map[committedPlacementKey]profilePlacement{},
	}}
}

func (s *QuotaTelemetryStore) Snapshot() QuotaTelemetrySnapshot {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.inner.buildSnapshot()
}

// DrawAndReserve atomically snapshots telemetry, draws/orders candidates,
// and reserves `candidates[0]` in-flight under a single short synchronous
// lock. Rust's `QuotaTelemetryStore::draw_and_reserve`: a function because
// Go methods cannot take type parameters. A nil reservation means no
// candidate survived.
func DrawAndReserve[T ScheduledCandidateRef](s *QuotaTelemetryStore, candidates *[]T, executionPolicy core.ExecutionPolicy, placement Placement, category *string, rankingAnchor *string, prefer []string, powerRanking *PowerRankingSnapshot, nowSeconds int64) (*PlacementReservation, error) {
	s.mu.Lock()
	defer s.mu.Unlock()
	snapshot := s.inner.buildSnapshot()
	ctx := SchedulingContext{
		ExecutionPolicy: executionPolicy,
		Placement:       placement,
		Category:        category,
		RankingAnchor:   rankingAnchor,
		Prefer:          prefer,
		Telemetry:       &snapshot,
		PowerRanking:    powerRanking,
		NowSeconds:      nowSeconds,
	}
	if err := DrawScheduledCandidates(candidates, &ctx); err != nil {
		return nil, err
	}
	if len(*candidates) == 0 {
		return nil, nil
	}
	first := (*candidates)[0].AsScheduledCandidate()
	reservationID := s.inner.nextReservationID
	if s.inner.nextReservationID != math.MaxUint64 {
		s.inner.nextReservationID++
	}
	recordedCategory := "primary"
	if category != nil {
		recordedCategory = *category
	}
	if s.inner.inFlight == nil {
		s.inner.inFlight = map[uint64]profilePlacement{}
	}
	s.inner.inFlight[reservationID] = profilePlacement{
		profileID: first.ProfileID,
		placement: RecentPlacement{Category: recordedCategory, CreatedAtEpochSeconds: nowSeconds},
	}
	return &PlacementReservation{store: s, reservationID: reservationID}, nil
}

func (s *QuotaTelemetryStore) ReplaceWindows(windows map[WindowKey]WindowTelemetry) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.inner.windows = windows
}

func (s *QuotaTelemetryStore) ReplaceRecentPlacements(placements map[string][]RecentPlacement) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.inner.durablePlacements = placements
}

func (s *QuotaTelemetryStore) RecordPlacement(profileID, category string, createdAtEpochSeconds int64) {
	s.mu.Lock()
	defer s.mu.Unlock()
	id := s.inner.nextReservationID
	if s.inner.nextReservationID != math.MaxUint64 {
		s.inner.nextReservationID++
	}
	recorded := fmt.Sprintf("__recorded-%d", id)
	if s.inner.committedPending == nil {
		s.inner.committedPending = map[committedPlacementKey]profilePlacement{}
	}
	s.inner.committedPending[committedPlacementKey{sessionID: recorded, profileID: recorded}] = profilePlacement{
		profileID: profileID,
		placement: RecentPlacement{Category: category, CreatedAtEpochSeconds: createdAtEpochSeconds},
	}
}

func (s *QuotaTelemetryStore) RecordCommittedPlacement(sessionID, profileID, category string, createdAtEpochSeconds int64) {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.inner.committedPending == nil {
		s.inner.committedPending = map[committedPlacementKey]profilePlacement{}
	}
	s.inner.committedPending[committedPlacementKey{sessionID: sessionID, profileID: profileID}] = profilePlacement{
		profileID: profileID,
		placement: RecentPlacement{Category: category, CreatedAtEpochSeconds: createdAtEpochSeconds},
	}
}

func (s *QuotaTelemetryStore) ReplaceCategoryStats(stats map[string]CategoryBurnStats) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.inner.categoryStats = stats
}

// RefreshFromStore recomputes `windows` and `durable_placements` from
// SQLite for the given `(profile_id, cache_identity)` pairs. Safe to call
// on the 10-minute quota poller background task (never on the spawn hot
// path).
//
// Deduplicates `committed_pending` by `(session_id, profile_id)` against
// the loaded SQLite rows so a placement committed concurrently with a
// refresh is never double-counted.
func (s *QuotaTelemetryStore) RefreshFromStore(profiles [][2]string, nowSeconds int64) error {
	since := uint64(0)
	if nowSeconds > 0 {
		since = uint64(nowSeconds)
	}
	if since > HistoryLookbackSeconds {
		since -= HistoryLookbackSeconds
	} else {
		since = 0
	}
	allWindows := map[WindowKey]WindowTelemetry{}
	for _, profile := range profiles {
		profileID, cacheIdentity := profile[0], profile[1]
		history, err := store.LoadQuotaHistorySince(cacheIdentity, profileID, since)
		if err != nil {
			return err
		}
		for key, value := range ComputeWindowTelemetry(profileID, cacheIdentity, history, nowSeconds) {
			allWindows[key] = value
		}
	}
	placementsSince := telemetrySaturatingSub(nowSeconds, int64(HistoryLookbackSeconds))
	rows, err := store.LoadRecentPlacementsSince(placementsSince)
	if err != nil {
		return err
	}
	s.ApplyStoreRefresh(allWindows, rows, placementsSince)
	return nil
}

func (s *QuotaTelemetryStore) ApplyStoreRefresh(allWindows map[WindowKey]WindowTelemetry, rows []store.RecentPlacementRow, placementsSince int64) {
	seen := map[committedPlacementKey]int64{}
	durablePlacements := map[string][]RecentPlacement{}
	for _, row := range rows {
		key := committedPlacementKey{sessionID: row.SessionID, profileID: row.ProfileID}
		if timestamp, ok := seen[key]; !ok || row.CreatedAtEpochSeconds > timestamp {
			seen[key] = row.CreatedAtEpochSeconds
		}
		durablePlacements[row.ProfileID] = append(durablePlacements[row.ProfileID], RecentPlacement{
			Category:              row.Category,
			CreatedAtEpochSeconds: row.CreatedAtEpochSeconds,
		})
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	for key, entry := range s.inner.committedPending {
		created := entry.placement.CreatedAtEpochSeconds
		rowTs, recorded := seen[key]
		if created >= placementsSince && (!recorded || rowTs < telemetrySaturatingSub(created, 60)) {
			continue
		}
		delete(s.inner.committedPending, key)
	}
	s.inner.windows = allWindows
	s.inner.durablePlacements = durablePlacements
}

// RefreshCategoryStatsFromStore lazily refreshes local category burn
// observations from SQLite (startup + hourly background task; never on the
// 10-minute poll or spawn hot path).
func (s *QuotaTelemetryStore) RefreshCategoryStatsFromStore() error {
	observed, err := store.LoadCategoryBurnObservations(LocalCategorySwitchoverMinSamples, LocalCategoryMaxSamples)
	if err != nil {
		return err
	}
	stats := make(map[string]CategoryBurnStats, len(observed))
	for category, observation := range observed {
		stats[category] = CategoryBurnStats{
			MeanAPIUsd:          observation.MeanAPIUsd,
			RunwayDurationHours: observation.P90DurationHours,
			LocalSampleCount:    observation.SampleCount,
		}
	}
	s.ReplaceCategoryStats(stats)
	return nil
}

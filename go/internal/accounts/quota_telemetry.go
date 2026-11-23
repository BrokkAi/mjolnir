// Port of mj-accounts/src/quota_telemetry.rs.

// Trailing-60-minute quota burn velocity, durable placement reservations,
// and category burn statistics for the unified runway gate.

package accounts

import (
	"cmp"
	"fmt"
	"math"
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
	panic("unported: mj-accounts/src/quota_telemetry.rs default_category_burn_stats")
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
	panic("unported: mj-accounts/src/quota_telemetry.rs QuotaTelemetrySnapshot::window_telemetry")
}

// ReservedPct sums `mean_burn_pct(placement.category, window_kind)` over
// placements on profileID created strictly after secondToLastCleanTs (or
// all recent placements if secondToLastCleanTs is nil).
func (s *QuotaTelemetrySnapshot) ReservedPct(profileID string, windowKind WindowKind, secondToLastCleanTs *int64, weeklyBudgetUsd *float64, pEff float64, planUsdPerMonth *float64, fiveHourToWeeklyRatio *float64) float64 {
	panic("unported: mj-accounts/src/quota_telemetry.rs QuotaTelemetrySnapshot::reserved_pct")
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
	panic("unported: mj-accounts/src/quota_telemetry.rs compute_window_telemetry")
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
	panic("unported: mj-accounts/src/quota_telemetry.rs QuotaTelemetryStoreInner::build_snapshot")
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
	panic("unported: mj-accounts/src/quota_telemetry.rs PlacementReservation::commit")
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
	panic("unported: mj-accounts/src/quota_telemetry.rs QuotaTelemetryStore::draw_and_reserve")
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
	panic("unported: mj-accounts/src/quota_telemetry.rs QuotaTelemetryStore::refresh_from_store")
}

func (s *QuotaTelemetryStore) ApplyStoreRefresh(allWindows map[WindowKey]WindowTelemetry, rows []store.RecentPlacementRow, placementsSince int64) {
	panic("unported: mj-accounts/src/quota_telemetry.rs QuotaTelemetryStore::apply_store_refresh")
}

// RefreshCategoryStatsFromStore lazily refreshes local category burn
// observations from SQLite (startup + hourly background task; never on the
// 10-minute poll or spawn hot path).
func (s *QuotaTelemetryStore) RefreshCategoryStatsFromStore() error {
	panic("unported: mj-accounts/src/quota_telemetry.rs QuotaTelemetryStore::refresh_category_stats_from_store")
}

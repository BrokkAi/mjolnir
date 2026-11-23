// Port of mj-store/src/database/usage.rs.

package store

import (
	"context"
	"database/sql"
	"errors"
	"math"
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func recordChildAccounting(connection Connection, child *core.SubagentRecord, category *string) error {
	panic("unported: mj-store/src/database/usage.rs record_child_accounting")
}

func recordTurnSelection(connection Connection, session, command string, configuration *core.SessionConfiguration) error {
	panic("unported: mj-store/src/database/usage.rs record_turn_selection")
}

func LoadSessionUsage(sessionID string, afterSeq uint64, limit int) (*core.UsagePage, error) {
	return loadSessionUsageFrom(DatabasePath(), sessionID, afterSeq, limit)
}

func contextExists(connection Connection, session string) (bool, error) {
	var exists bool
	err := connection.QueryRowContext(context.Background(),
		"SELECT EXISTS(SELECT 1 FROM session_contexts WHERE session_id=?1)", session).Scan(&exists)
	return exists, err
}

func loadSessionUsageFrom(path, sessionID string, afterSeq uint64, limit int) (*core.UsagePage, error) {
	panic("unported: mj-store/src/database/usage.rs load_session_usage_from")
}

func selectionFor(connection Connection, session, command string) (core.UsageSelection, error) {
	var selection core.UsageSelection
	err := connection.QueryRowContext(context.Background(),
		"SELECT model, effort FROM session_turn_selections WHERE session_id=?1 AND command_id=?2",
		session, command).Scan(&selection.Model, &selection.Effort)
	if errors.Is(err, sql.ErrNoRows) {
		return core.UsageSelection{}, nil
	}
	return selection, err
}

// LoadSessionTurnProfileModel is the profile and model selected when a turn
// started, if its selection row has committed. A same-page failure can
// precede that row's projection commit, so the profile falls back to the
// session while the model stays absent. False when the session is unknown.
func LoadSessionTurnProfileModel(session, command string) (string, *string, bool, error) {
	panic("unported: mj-store/src/database/usage.rs load_session_turn_profile_model")
}

// LoadSessionTurnProfileCurrent is the recorded turn profile and the
// session's current profile from one snapshot. False when the session is
// unknown.
func LoadSessionTurnProfileCurrent(session, command string) (string, string, bool, error) {
	panic("unported: mj-store/src/database/usage.rs load_session_turn_profile_current")
}

func counters(usage *core.TokenUsage) map[string]uint64 {
	values := map[string]uint64{
		"total_tokens":  usage.TotalTokens,
		"input_tokens":  usage.InputTokens,
		"output_tokens": usage.OutputTokens,
	}
	for name, value := range map[string]*uint64{
		"thought_tokens":      usage.ThoughtTokens,
		"cached_read_tokens":  usage.CachedReadTokens,
		"cached_write_tokens": usage.CachedWriteTokens,
	} {
		if value != nil {
			values[name] = *value
		}
	}
	return values
}

func estimateModelCost(usage *core.TokenUsage, model *string, totals map[string]core.UsageCounterTotal) (*uint64, error) {
	// An unattributed cache counter is unknown, not zero.
	_, cachedRead := totals["cached_read_tokens"]
	_, cachedWrite := totals["cached_write_tokens"]
	if (usage.CachedReadTokens != nil && !cachedRead) || (usage.CachedWriteTokens != nil && !cachedWrite) {
		return nil, nil
	}
	return core.Estimate(model, totals)
}

// addCounters adds one full-turn report's counters to totals, which must not
// be nil.
func addCounters(totals map[string]core.UsageCounterTotal, values map[string]uint64) error {
	for name, tokens := range values {
		total := totals[name]
		if total.Tokens > math.MaxUint64-tokens {
			return errors.New("usage token total overflow")
		}
		if total.ReportedTurns == math.MaxUint64 {
			return errors.New("usage turn total overflow")
		}
		total.Tokens += tokens
		total.ReportedTurns++
		totals[name] = total
	}
	return nil
}

// usageGroup is one entry of usageGroups.
type usageGroup struct {
	selection core.UsageSelection
	totals    map[string]core.UsageCounterTotal
}

// usageGroups is Rust's BTreeMap<UsageSelection, BTreeMap<String,
// UsageCounterTotal>>: counter totals per selection, kept in the order
// UsageSelection.Compare defines.
type usageGroups []usageGroup

// entry is the selection's totals, inserted empty when absent (Rust's
// entry(..).or_default()).
func (g *usageGroups) entry(selection core.UsageSelection) map[string]core.UsageCounterTotal {
	index, found := slices.BinarySearchFunc(*g, selection, func(group usageGroup, target core.UsageSelection) int {
		return group.selection.Compare(target)
	})
	if !found {
		*g = slices.Insert(*g, index, usageGroup{selection: selection, totals: map[string]core.UsageCounterTotal{}})
	}
	return (*g)[index].totals
}

// attribute attributes each counter once. Partial or inconsistent breakdowns
// retain an unknown remainder instead of assigning it to the configured
// model.
func attribute(usage *core.TokenUsage, selected core.UsageSelection, groups *usageGroups) error {
	panic("unported: mj-store/src/database/usage.rs attribute")
}

func sessionSummary(connection Connection, session string) (core.UsageSummary, error) {
	panic("unported: mj-store/src/database/usage.rs session_summary")
}

func LoadUsageTree(parent string) (*core.UsageTree, error) {
	return loadUsageTreeFrom(DatabasePath(), parent)
}

func loadUsageTreeFrom(path, parent string) (*core.UsageTree, error) {
	panic("unported: mj-store/src/database/usage.rs load_usage_tree_from")
}

// mergeTotals adds source's totals into target, which must not be nil.
func mergeTotals(target, source map[string]core.UsageCounterTotal) error {
	for name, value := range source {
		total := target[name]
		if total.Tokens > math.MaxUint64-value.Tokens {
			return errors.New("usage tree token overflow")
		}
		if total.ReportedTurns > math.MaxUint64-value.ReportedTurns {
			return errors.New("usage tree turn overflow")
		}
		total.Tokens += value.Tokens
		total.ReportedTurns += value.ReportedTurns
		target[name] = total
	}
	return nil
}

func mergeSummary(target, source *core.UsageSummary) error {
	panic("unported: mj-store/src/database/usage.rs merge_summary")
}

// RecentPlacementRow is one durable session placement loaded from SQLite for
// quota reservation accounting.
type RecentPlacementRow struct {
	SessionID             string
	ProfileID             string
	Category              string
	CreatedAtEpochSeconds int64
}

// CategoryBurnObservation is locally observed category burn statistics from
// completed sessions.
type CategoryBurnObservation struct {
	MeanAPIUsd       float64
	P90DurationHours float64
	SampleCount      int
}

var observedCategories = []string{
	"straightforward",
	"subtle",
	"reviewer",
	"brief_writer",
	"oracle",
	"orchestrator",
	"primary",
}

// LoadRecentPlacementsSince loads session placements created at or after
// sinceEpochSeconds from sessions + session_contexts + subagent_accounting.
func LoadRecentPlacementsSince(sinceEpochSeconds int64) ([]RecentPlacementRow, error) {
	return LoadRecentPlacementsSinceFrom(DatabasePath(), sinceEpochSeconds)
}

func LoadRecentPlacementsSinceFrom(path string, sinceEpochSeconds int64) ([]RecentPlacementRow, error) {
	panic("unported: mj-store/src/database/usage.rs load_recent_placements_since_from")
}

func recentPlacementsSince(connection Connection, sinceEpochSeconds int64) ([]RecentPlacementRow, error) {
	panic("unported: mj-store/src/database/usage.rs recent_placements_since")
}

// LoadCategoryBurnObservations is the bounded query for local category burn
// switchover (N >= minSamples). Intended for lazy startup / hourly background
// refresh, never hot paths.
func LoadCategoryBurnObservations(minSamples, maxPerCategory int) (map[string]CategoryBurnObservation, error) {
	return LoadCategoryBurnObservationsFrom(DatabasePath(), minSamples, maxPerCategory)
}

func LoadCategoryBurnObservationsFrom(path string, minSamples, maxPerCategory int) (map[string]CategoryBurnObservation, error) {
	panic("unported: mj-store/src/database/usage.rs load_category_burn_observations_from")
}

func categoryBurnObservations(connection Connection, minSamples, maxPerCategory int) (map[string]CategoryBurnObservation, error) {
	panic("unported: mj-store/src/database/usage.rs category_burn_observations")
}

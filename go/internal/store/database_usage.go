// Port of mj-store/src/database/usage.rs.

package store

import (
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"math"
	"slices"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func recordChildAccounting(connection Queryer, child *core.SubagentRecord, category *string) error {
	// Named columns, so an older writer that omits `category` leaves it NULL
	// and does not clear a value this build already stored.
	if _, err := connection.Exec(`INSERT INTO subagent_accounting(
            child_session_id, parent_session_id, task_name, category
        ) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(child_session_id) DO NOTHING`,
		child.ChildSessionID, child.ParentSessionID, child.TaskName, category); err != nil {
		return err
	}
	var parent, task string
	if err := connection.QueryRow("SELECT parent_session_id, task_name FROM subagent_accounting WHERE child_session_id=?1",
		child.ChildSessionID).Scan(&parent, &task); err != nil {
		return err
	}
	if parent != child.ParentSessionID || task != child.TaskName {
		return fmt.Errorf("accounting identity changed for child %s", child.ChildSessionID)
	}
	return nil
}

func recordTurnSelection(connection Queryer, session, command string, configuration *core.SessionConfiguration) error {
	surface := core.AcpSessionSurfaceFromConfiguration(configuration.Values)
	// A reviewer's launch settings are the authority for its turn. Its
	// projected configuration can record the model without the effort the
	// harness was started with, so read the registered child's selection as
	// the fallback. The relation still exists while the reviewer runs.
	var storedModel, storedEffort *string
	err := connection.QueryRow(`SELECT json_extract(record_json, '$.model'),
                    json_extract(record_json, '$.effort')
             FROM subagent_sessions WHERE child_session_id=?1`, session).Scan(&storedModel, &storedEffort)
	if err != nil && !errors.Is(err, sql.ErrNoRows) {
		return err
	}
	model := storedModel
	if current, ok := surface.CurrentModel(); ok {
		model = &current
	}
	effort := storedEffort
	if current, ok := surface.CurrentEffort(); ok {
		effort = &current
	}
	// The session's profile at turn start: a later restart can switch it,
	// and destroying a sub-agent deletes its session row. A child copies its
	// accounting category, including NULL from a registration that predates
	// the column. A session with no accounting row is top-level, so it is
	// `primary`. Older writers name columns and leave `category` untouched.
	_, err = connection.Exec(`INSERT INTO session_turn_selections(
            session_id, command_id, model, effort, profile, category
        ) VALUES (
            ?1, ?2, ?3, ?4,
            (SELECT last_profile FROM sessions WHERE session_id=?1),
            CASE
                WHEN EXISTS (
                    SELECT 1 FROM subagent_accounting WHERE child_session_id=?1
                )
                THEN (
                    SELECT category FROM subagent_accounting WHERE child_session_id=?1
                )
                ELSE 'primary'
            END
        )
        ON CONFLICT(session_id, command_id) DO NOTHING`, session, command, model, effort)
	return err
}

func LoadSessionUsage(sessionID string, afterSeq uint64, limit int) (*core.UsagePage, error) {
	return loadSessionUsageFrom(DatabasePath(), sessionID, afterSeq, limit)
}

func contextExists(connection Queryer, session string) (bool, error) {
	var exists bool
	err := connection.QueryRow("SELECT EXISTS(SELECT 1 FROM session_contexts WHERE session_id=?1)", session).Scan(&exists)
	return exists, err
}

func loadSessionUsageFrom(path, sessionID string, afterSeq uint64, limit int) (*core.UsagePage, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	tx, err := connection.Begin()
	if err != nil {
		return nil, err
	}
	defer tx.Rollback()
	exists, err := contextExists(tx, sessionID)
	if err != nil || !exists {
		return nil, err
	}
	var latestSeq uint64
	if err := tx.QueryRow("SELECT COALESCE(MAX(completed_ordinal), 0) FROM session_turn_usage WHERE session_id=?1",
		sessionID).Scan(&latestSeq); err != nil {
		return nil, err
	}
	bodies, err := committedStrings(tx, "SELECT body FROM session_turn_usage WHERE session_id=?1 AND completed_ordinal > ?2 ORDER BY completed_ordinal LIMIT ?3",
		sessionID, afterSeq, int64(min(max(limit, 1), 1000)))
	if err != nil {
		return nil, err
	}
	turns := make([]core.MaterializedTurnOutcome, 0, len(bodies))
	for _, body := range bodies {
		turn, err := deserializeTurnOutcome(body)
		if err != nil {
			return nil, err
		}
		turns = append(turns, turn)
	}
	turnSelections := map[string]core.UsageSelection{}
	for _, turn := range turns {
		selection, err := selectionFor(tx, sessionID, turn.CommandID)
		if err != nil {
			return nil, err
		}
		turnSelections[turn.CommandID] = selection
	}
	summary, err := sessionSummary(tx, sessionID)
	if err != nil {
		return nil, err
	}
	nextAfterSeq := max(latestSeq, afterSeq)
	if len(turns) > 0 {
		nextAfterSeq = turns[len(turns)-1].CompletedOrdinal
	}
	return &core.UsagePage{
		SessionID:           sessionID,
		NextAfterSeq:        nextAfterSeq,
		LatestSeq:           latestSeq,
		Turns:               turns,
		Totals:              summary.Totals,
		Coverage:            summary.Coverage,
		ProviderSessionCost: summary.ProviderSessionCost,
		TurnCostUsdTicks:    summary.TurnCostUsdTicks,
		TurnSelections:      turnSelections,
		ByModel:             summary.ByModel,
	}, nil
}

func selectionFor(connection Queryer, session, command string) (core.UsageSelection, error) {
	var selection core.UsageSelection
	err := connection.QueryRow("SELECT model, effort FROM session_turn_selections WHERE session_id=?1 AND command_id=?2",
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
	connection, err := openReader(DatabasePath())
	if err != nil {
		return "", nil, false, err
	}
	defer connection.Close()
	var profile string
	var model *string
	err = connection.QueryRow(`SELECT COALESCE(t.profile, s.last_profile), t.model FROM sessions s
             LEFT JOIN session_turn_selections t ON t.session_id=s.session_id AND t.command_id=?2
             WHERE s.session_id=?1`, session, command).Scan(&profile, &model)
	if errors.Is(err, sql.ErrNoRows) {
		return "", nil, false, nil
	}
	if err != nil {
		return "", nil, false, err
	}
	return profile, model, true, nil
}

// LoadSessionTurnProfileCurrent is the recorded turn profile and the
// session's current profile from one snapshot. False when the session is
// unknown.
func LoadSessionTurnProfileCurrent(session, command string) (string, string, bool, error) {
	connection, err := openReader(DatabasePath())
	if err != nil {
		return "", "", false, err
	}
	defer connection.Close()
	var recorded, current string
	err = connection.QueryRow(`SELECT COALESCE(t.profile, s.last_profile), s.last_profile FROM sessions s
         LEFT JOIN session_turn_selections t ON t.session_id=s.session_id AND t.command_id=?2
         WHERE s.session_id=?1`, session, command).Scan(&recorded, &current)
	if errors.Is(err, sql.ErrNoRows) {
		return "", "", false, nil
	}
	if err != nil {
		return "", "", false, err
	}
	return recorded, current, true, nil
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
	for _, name := range slices.Sorted(maps.Keys(values)) {
		tokens := values[name]
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

// modelTotals is the groups as the summary lists them.
func (g usageGroups) modelTotals() []core.UsageModelTotal {
	totals := make([]core.UsageModelTotal, 0, len(g))
	for _, group := range g {
		totals = append(totals, core.UsageModelTotal{Selection: group.selection, Totals: group.totals})
	}
	return totals
}

// usageAssignment is one entry of attribute's per-selection counter values,
// kept in selection order.
type usageAssignment struct {
	selection core.UsageSelection
	values    map[string]uint64
}

func usageAssignmentEntry(assigned *[]usageAssignment, selection core.UsageSelection) map[string]uint64 {
	index, found := slices.BinarySearchFunc(*assigned, selection, func(entry usageAssignment, target core.UsageSelection) int {
		return entry.selection.Compare(target)
	})
	if !found {
		*assigned = slices.Insert(*assigned, index, usageAssignment{selection: selection, values: map[string]uint64{}})
	}
	return (*assigned)[index].values
}

// attribute attributes each counter once. Partial or inconsistent breakdowns
// retain an unknown remainder instead of assigning it to the configured
// model.
func attribute(usage *core.TokenUsage, selected core.UsageSelection, groups *usageGroups) error {
	values := counters(usage)
	if usage.ProviderDetails == nil || len(usage.ProviderDetails.ModelUsage) == 0 {
		return addCounters(groups.entry(selected), values)
	}
	models := usage.ProviderDetails.ModelUsage
	modelNames := slices.Sorted(maps.Keys(models))
	type part struct {
		model string
		scope core.UsageScope
		count *uint64
	}
	var assigned []usageAssignment
	for _, counter := range slices.Sorted(maps.Keys(values)) {
		total := values[counter]
		parts := make([]part, 0, len(modelNames))
		for _, model := range modelNames {
			report := models[model]
			var count *uint64
			if value, ok := counters(&report)[counter]; ok {
				count = &value
			}
			parts = append(parts, part{model: model, scope: report.Scope, count: count})
		}
		var sum uint64
		valid := true
		for _, part := range parts {
			if part.scope != core.UsageScopeTurn || part.count == nil || sum > math.MaxUint64-*part.count {
				valid = false
				break
			}
			sum += *part.count
		}
		if valid && sum <= total {
			for _, part := range parts {
				model := part.model
				selection := core.UsageSelection{Model: &model}
				if selected.Model != nil && *selected.Model == model {
					selection.Effort = selected.Effort
				}
				usageAssignmentEntry(&assigned, selection)[counter] = *part.count
			}
			if sum < total {
				usageAssignmentEntry(&assigned, core.UsageSelection{})[counter] = total - sum
			}
		} else {
			usageAssignmentEntry(&assigned, core.UsageSelection{})[counter] = total
		}
	}
	for _, assignment := range assigned {
		if err := addCounters(groups.entry(assignment.selection), assignment.values); err != nil {
			return err
		}
	}
	return nil
}

func sessionSummary(connection Queryer, session string) (core.UsageSummary, error) {
	summary := core.UsageSummary{Totals: map[string]core.UsageCounterTotal{}}
	var groups usageGroups
	type usageRow struct {
		body      string
		selection core.UsageSelection
	}
	var turnRows []usageRow
	err := func() error {
		rows, err := connection.Query(`SELECT u.body, s.model, s.effort FROM session_turn_usage u
        LEFT JOIN session_turn_selections s USING(session_id, command_id) WHERE u.session_id=?1 ORDER BY u.completed_ordinal`, session)
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			var row usageRow
			if err := rows.Scan(&row.body, &row.selection.Model, &row.selection.Effort); err != nil {
				return err
			}
			turnRows = append(turnRows, row)
		}
		return rows.Err()
	}()
	if err != nil {
		return core.UsageSummary{}, err
	}
	for _, row := range turnRows {
		turn, err := deserializeTurnOutcome(row.body)
		if err != nil {
			return core.UsageSummary{}, err
		}
		summary.Coverage.RecordedTurns++
		usage := turn.Usage
		switch {
		case usage != nil && usage.Scope == core.UsageScopeTurn:
			summary.Coverage.FullTurnReports++
			if err := addCounters(summary.Totals, counters(usage)); err != nil {
				return core.UsageSummary{}, err
			}
			var providerCost *core.ProviderTurnCost
			if usage.ProviderDetails != nil && usage.ProviderDetails.Cost != nil && !usage.ProviderDetails.Cost.IsPartial {
				providerCost = usage.ProviderDetails.Cost
			}
			var estimated *uint64
			if providerCost == nil {
				var attributed usageGroups
				if err := attribute(usage, row.selection, &attributed); err != nil {
					return core.UsageSummary{}, err
				}
				var sum uint64
				known := len(attributed) > 0
				for _, group := range attributed {
					part, err := estimateModelCost(usage, group.selection.Model, group.totals)
					if err != nil {
						return core.UsageSummary{}, err
					}
					if part == nil {
						known = false
						continue
					}
					if sum > math.MaxUint64-*part {
						return core.UsageSummary{}, errors.New("turn cost overflow")
					}
					sum += *part
				}
				if known {
					estimated = &sum
				}
			}
			var cost *uint64
			switch {
			case providerCost != nil:
				summary.Coverage.ProviderCostTurns++
				ticks := providerCost.UsdTicks
				cost = &ticks
			case estimated != nil:
				summary.Coverage.EstimatedCostTurns++
				cost = estimated
			default:
				summary.Coverage.MissingCostTurns++
			}
			if cost != nil {
				if summary.TurnCostUsdTicks > math.MaxUint64-*cost {
					return core.UsageSummary{}, errors.New("turn cost overflow")
				}
				summary.TurnCostUsdTicks += *cost
			}
			if err := attribute(usage, row.selection, &groups); err != nil {
				return core.UsageSummary{}, err
			}
		case usage != nil && usage.Scope == core.UsageScopeLastRequest:
			summary.Coverage.LastRequestReports++
		case usage != nil:
			summary.Coverage.UnspecifiedReports++
		default:
			summary.Coverage.MissingReports++
		}
	}
	if err := connection.QueryRow(`SELECT COUNT(*) FROM session_turn_selections s
        WHERE session_id=?1 AND NOT EXISTS(SELECT 1 FROM session_turn_usage u WHERE u.session_id=s.session_id AND u.command_id=s.command_id)`,
		session).Scan(&summary.Coverage.UnfinishedTurns); err != nil {
		return core.UsageSummary{}, err
	}
	summary.ByModel = groups.modelTotals()
	cost, err := environmentOptionalString(connection, "SELECT body FROM session_provider_cost WHERE session_id=?1", session)
	if err != nil {
		return core.UsageSummary{}, err
	}
	if cost != nil {
		var providerCost core.ProviderCost
		if err := json.Unmarshal([]byte(*cost), &providerCost); err != nil {
			return core.UsageSummary{}, err
		}
		summary.ProviderSessionCost = &providerCost
	}
	return summary, nil
}

func LoadUsageTree(parent string) (*core.UsageTree, error) {
	return loadUsageTreeFrom(DatabasePath(), parent)
}

func loadUsageTreeFrom(path, parent string) (*core.UsageTree, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	tx, err := connection.Begin()
	if err != nil {
		return nil, err
	}
	defer tx.Rollback()
	exists, err := contextExists(tx, parent)
	if err != nil || !exists {
		return nil, err
	}
	type identity struct {
		sessionID       string
		parentSessionID *string
		taskName        *string
		present         bool
	}
	var identities []identity
	err = func() error {
		rows, err := tx.Query(`WITH RECURSIVE tree(id) AS (
        SELECT ?1 UNION SELECT a.child_session_id FROM subagent_accounting a JOIN tree t ON a.parent_session_id=t.id)
        SELECT t.id, a.parent_session_id, a.task_name, EXISTS(SELECT 1 FROM sessions s WHERE s.session_id=t.id)
        FROM tree t LEFT JOIN subagent_accounting a ON a.child_session_id=t.id ORDER BY t.id`, parent)
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			var row identity
			if err := rows.Scan(&row.sessionID, &row.parentSessionID, &row.taskName, &row.present); err != nil {
				return err
			}
			identities = append(identities, row)
		}
		return rows.Err()
	}()
	if err != nil {
		return nil, err
	}
	summary := core.UsageSummary{Totals: map[string]core.UsageCounterTotal{}}
	var groups usageGroups
	sessions := make([]core.UsageTreeSession, 0, len(identities))
	for _, member := range identities {
		memberSummary, err := sessionSummary(tx, member.sessionID)
		if err != nil {
			return nil, err
		}
		if err := mergeSummary(&summary, &memberSummary); err != nil {
			return nil, err
		}
		for _, group := range memberSummary.ByModel {
			if err := mergeTotals(groups.entry(group.Selection), group.Totals); err != nil {
				return nil, err
			}
		}
		sessions = append(sessions, core.UsageTreeSession{
			SessionID:                 member.sessionID,
			ParentSessionID:           member.parentSessionID,
			TaskName:                  member.taskName,
			OperationalSessionPresent: member.present,
			Summary:                   memberSummary,
		})
	}
	summary.ByModel = groups.modelTotals()
	return &core.UsageTree{ParentSessionID: parent, Sessions: sessions, Summary: summary}, nil
}

// mergeTotals adds source's totals into target, which must not be nil.
func mergeTotals(target, source map[string]core.UsageCounterTotal) error {
	for _, name := range slices.Sorted(maps.Keys(source)) {
		value := source[name]
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
	if target.Totals == nil {
		target.Totals = map[string]core.UsageCounterTotal{}
	}
	if err := mergeTotals(target.Totals, source.Totals); err != nil {
		return err
	}
	if target.TurnCostUsdTicks > math.MaxUint64-source.TurnCostUsdTicks {
		return errors.New("usage tree cost overflow")
	}
	target.TurnCostUsdTicks += source.TurnCostUsdTicks
	a, b := &target.Coverage, &source.Coverage
	for _, pair := range []struct {
		target *uint64
		value  uint64
	}{
		{&a.RecordedTurns, b.RecordedTurns},
		{&a.FullTurnReports, b.FullTurnReports},
		{&a.LastRequestReports, b.LastRequestReports},
		{&a.UnspecifiedReports, b.UnspecifiedReports},
		{&a.MissingReports, b.MissingReports},
		{&a.UnfinishedTurns, b.UnfinishedTurns},
		{&a.ProviderCostTurns, b.ProviderCostTurns},
		{&a.EstimatedCostTurns, b.EstimatedCostTurns},
		{&a.MissingCostTurns, b.MissingCostTurns},
	} {
		if *pair.target > math.MaxUint64-pair.value {
			return errors.New("usage coverage overflow")
		}
		*pair.target += pair.value
	}
	return nil
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
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return recentPlacementsSince(connection, sinceEpochSeconds)
}

// chronoTimestampBounds are the first and last whole seconds chrono's
// DateTime::from_timestamp accepts (-262143-01-01 through
// +262142-12-31T23:59:59).
const (
	chronoMinTimestamp int64 = -8_334_601_228_800
	chronoMaxTimestamp int64 = 8_210_266_876_799
)

// usageRFC3339Seconds is chrono's DateTime::from_timestamp(seconds, 0),
// falling back to the epoch outside its range, written with
// to_rfc3339_opts(SecondsFormat::Secs, true).
func usageRFC3339Seconds(seconds int64) string {
	if seconds < chronoMinTimestamp || seconds > chronoMaxTimestamp {
		seconds = 0
	}
	t := time.Unix(seconds, 0).UTC()
	year := t.Year()
	rest := t.Format("-01-02T15:04:05Z")
	if year >= 0 && year <= 9999 {
		return fmt.Sprintf("%04d%s", year, rest)
	}
	return fmt.Sprintf("%+05d%s", year, rest)
}

func recentPlacementsSince(connection Queryer, sinceEpochSeconds int64) ([]RecentPlacementRow, error) {
	rows, err := environmentTextRows(connection, 4, `SELECT
            sc.session_id,
            COALESCE(s.last_profile, sc.profile_id),
            COALESCE(sa.category, 'primary'),
            sc.created_at
         FROM session_contexts sc
         LEFT JOIN sessions s ON s.session_id = sc.session_id
         LEFT JOIN subagent_accounting sa ON sa.child_session_id = sc.session_id
         WHERE sc.created_at >= ?1
           AND COALESCE(s.last_profile, sc.profile_id) IS NOT NULL
         ORDER BY sc.created_at ASC`, usageRFC3339Seconds(sinceEpochSeconds))
	if err != nil {
		return nil, err
	}
	placements := make([]RecentPlacementRow, 0, len(rows))
	for _, row := range rows {
		parsed, ok := core.ParseChronoRFC3339(row[3])
		if !ok {
			continue
		}
		if timestamp := parsed.Unix(); timestamp >= sinceEpochSeconds {
			placements = append(placements, RecentPlacementRow{
				SessionID:             row[0],
				ProfileID:             row[1],
				Category:              row[2],
				CreatedAtEpochSeconds: timestamp,
			})
		}
	}
	return placements, nil
}

// LoadCategoryBurnObservations is the bounded query for local category burn
// switchover (N >= minSamples). Intended for lazy startup / hourly background
// refresh, never hot paths.
func LoadCategoryBurnObservations(minSamples, maxPerCategory int) (map[string]CategoryBurnObservation, error) {
	return LoadCategoryBurnObservationsFrom(DatabasePath(), minSamples, maxPerCategory)
}

func LoadCategoryBurnObservationsFrom(path string, minSamples, maxPerCategory int) (map[string]CategoryBurnObservation, error) {
	connection, err := openReader(path)
	if err != nil {
		return nil, err
	}
	defer connection.Close()
	return categoryBurnObservations(connection, minSamples, maxPerCategory)
}

// saturatingSub is i64::saturating_sub.
func saturatingSub(a, b int64) int64 {
	difference := a - b
	if (b > 0 && difference > a) || (b < 0 && difference < a) {
		if b > 0 {
			return math.MinInt64
		}
		return math.MaxInt64
	}
	return difference
}

func categoryBurnObservations(connection Queryer, minSamples, maxPerCategory int) (map[string]CategoryBurnObservation, error) {
	results := map[string]CategoryBurnObservation{}
	limit := int64(maxPerCategory)
	for _, category := range observedCategories {
		sessions, err := environmentTextRows(connection, 3, `SELECT s.session_id, sc.created_at, s.updated_at
         FROM sessions s
         JOIN session_contexts sc ON sc.session_id = s.session_id
         WHERE s.state IN ('stopped', 'error', 'lost')
           AND EXISTS (
               SELECT 1 FROM session_turn_selections sts
               WHERE sts.session_id = s.session_id
                 AND (sts.category = ?1 OR (?1 = 'orchestrator' AND sts.category = 'supervisor'))
           )
         ORDER BY sc.created_at DESC
         LIMIT ?2`, category, limit)
		if err != nil {
			return nil, err
		}
		if len(sessions) < minSamples {
			continue
		}
		usdSamples := make([]float64, 0, len(sessions))
		durationHours := make([]float64, 0, len(sessions))
		for _, session := range sessions {
			sessionID, createdAt, updatedAt := session[0], session[1], session[2]
			start, startOK := core.ParseChronoRFC3339(createdAt)
			end, endOK := core.ParseChronoRFC3339(updatedAt)
			if startOK && endOK {
				seconds := max(saturatingSub(end.Unix(), start.Unix()), 60)
				durationHours = append(durationHours, float64(seconds)/3600.0)
			}
			type turnRow struct {
				body  string
				model *string
			}
			var turns []turnRow
			err := func() error {
				rows, err := connection.Query(`SELECT u.body, sts.model
         FROM session_turn_usage u
         LEFT JOIN session_turn_selections sts
           ON sts.session_id = u.session_id AND sts.command_id = u.command_id
         WHERE u.session_id = ?1`, sessionID)
				if err != nil {
					return err
				}
				defer rows.Close()
				for rows.Next() {
					var row turnRow
					if err := rows.Scan(&row.body, &row.model); err != nil {
						return err
					}
					turns = append(turns, row)
				}
				return rows.Err()
			}()
			if err != nil {
				return nil, err
			}
			var sessionTicks uint64
			hasCost := false
			for _, turn := range turns {
				var outcome core.ApiTurnOutcome
				if json.Unmarshal([]byte(turn.body), &outcome) != nil {
					continue
				}
				if usage := outcome.Usage; usage != nil {
					totals := map[string]core.UsageCounterTotal{}
					_ = addCounters(totals, counters(usage))
					if estimate, err := estimateModelCost(usage, turn.model, totals); err == nil && estimate != nil {
						if sessionTicks > math.MaxUint64-*estimate {
							sessionTicks = math.MaxUint64
						} else {
							sessionTicks += *estimate
						}
						hasCost = true
					}
				}
			}
			if hasCost {
				usdSamples = append(usdSamples, float64(sessionTicks)/1_000_000_000.0)
			}
		}
		if len(usdSamples) >= minSamples && len(durationHours) >= minSamples {
			var sum float64
			for _, sample := range usdSamples {
				sum += sample
			}
			slices.Sort(durationHours)
			p90 := int(math.Ceil(float64(len(durationHours))*0.90)) - 1
			p90 = min(max(p90, 0), len(durationHours)-1)
			results[category] = CategoryBurnObservation{
				MeanAPIUsd:       sum / float64(len(usdSamples)),
				P90DurationHours: durationHours[p90],
				SampleCount:      min(len(usdSamples), len(durationHours)),
			}
		}
	}
	return results, nil
}

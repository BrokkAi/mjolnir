// Port of mj-store/src/database/usage_report.rs.

// One consistent store snapshot of parent usage and retained descendants.

package store

import (
	"errors"
	"fmt"
	"maps"
	"math"
	"slices"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// reportTurnsSQL matches the expressions indexed by migration 125. Read the
// selected time window once, regardless of how many historical descendants
// its roots retain.
const reportTurnsSQL = `SELECT u.session_id, u.body, s.model, s.effort, s.profile,
            COALESCE(s.category, a.category), a.kind
    FROM session_turn_usage u
    LEFT JOIN session_turn_selections s USING(session_id, command_id)
    LEFT JOIN subagent_accounting a ON a.child_session_id=u.session_id
    WHERE json_extract(u.body, '$.completed_at_ms') >= ?1
    AND json_extract(u.body, '$.completed_at_ms') <= ?2
    AND (?3 IS NULL OR s.profile=?3)`

const reportActiveSQL = `SELECT m.session_id FROM materialized_sessions m
    LEFT JOIN session_turn_selections s ON s.session_id=m.session_id
        AND s.command_id=json_extract(m.active_turn_json, '$.command_id')
    WHERE m.active_turn_json IS NOT NULL
    AND json_extract(m.active_turn_json, '$.started_at_ms') <= ?1
    AND (?2 IS NULL OR s.profile=?2)`

// LoadSessionUsageReport reads the report. models is each profile's newest
// advertised model list, used to price model aliases.
func LoadSessionUsageReport(query *core.SessionUsageQuery, models map[string][]core.SessionConfigChoice) (core.SessionUsageReport, error) {
	return loadWith(DatabasePath(), query, time.Now().UnixMilli(), core.ModelCatalogs(models))
}

func childUsageRole(member string, category, kind *string) (core.UsageAgentRole, error) {
	if category != nil {
		switch *category {
		case "supervisor", "orchestrator":
			return core.UsageAgentRoleOrchestrator, nil
		case "subtle":
			return core.UsageAgentRoleSubtle, nil
		case "straightforward":
			return core.UsageAgentRoleStraightforward, nil
		case "oracle":
			return core.UsageAgentRoleOracle, nil
		case "reviewer", "brief_writer":
			return core.UsageAgentRoleReviewer, nil
		}
	}
	if kind == nil {
		return core.UsageAgentRoleUnclassifiedSubagent, nil
	}
	switch *kind {
	case "oracle":
		return core.UsageAgentRoleOracle, nil
	case "reviewer":
		return core.UsageAgentRoleReviewer, nil
	case "worker":
		return core.UsageAgentRoleSubagent, nil
	}
	return "", fmt.Errorf("unknown usage agent kind %q for %s", *kind, member)
}

// loadFrom is the test entry point (cfg(test) in Rust): no model catalogs.
func loadFrom(path string, query *core.SessionUsageQuery, nowMs int64) (core.SessionUsageReport, error) {
	return loadWith(path, query, nowMs, core.ModelCatalogs{})
}

// usageModelKey is a model group's key, Rust's Option<String>.
type usageModelKey struct {
	known bool
	name  string
}

func usageModelKeyOf(model *string) usageModelKey {
	if model == nil {
		return usageModelKey{}
	}
	return usageModelKey{known: true, name: *model}
}

// usageModelGroups is Rust's BTreeMap<Option<String>, ModelUsageEstimate>.
type usageModelGroups map[usageModelKey]*core.ModelUsageEstimate

// add merges one priced share of a turn into the model's group.
func (groups usageModelGroups) add(model *string, cost *uint64, totals map[string]core.UsageCounterTotal) error {
	key := usageModelKeyOf(model)
	group := groups[key]
	if group == nil {
		var name *string
		if model != nil {
			value := *model
			name = &value
		}
		group = &core.ModelUsageEstimate{Model: name, Totals: map[string]core.UsageCounterTotal{}}
		groups[key] = group
	}
	share := core.CostUsdTicks{Unknown: true}
	if cost != nil {
		share = core.KnownCost(*cost)
	}
	sum, err := core.AddCost(group.CostUsdTicks, share)
	if err != nil {
		return errors.New("model cost overflow")
	}
	group.CostUsdTicks = sum
	return mergeTotals(group.Totals, totals)
}

// sorted is the groups from the highest cost down, ties by model.
func (groups usageModelGroups) sorted() []core.ModelUsageEstimate {
	models := make([]core.ModelUsageEstimate, 0, len(groups))
	for _, group := range groups {
		models = append(models, *group)
	}
	slices.SortStableFunc(models, func(a, b core.ModelUsageEstimate) int {
		if order := core.DescendingCost(a.CostUsdTicks, b.CostUsdTicks); order != 0 {
			return order
		}
		return usageCompareModels(a.Model, b.Model)
	})
	return models
}

// usageCompareModels orders Option<String>: none first, then by bytes.
func usageCompareModels(a, b *string) int {
	switch {
	case a == nil && b == nil:
		return 0
	case a == nil:
		return -1
	case b == nil:
		return 1
	}
	return strings.Compare(*a, *b)
}

// usageSumCosts adds the models' costs; an unknown one makes the sum
// unknown.
func usageSumCosts(models []core.ModelUsageEstimate, overflow string) (core.CostUsdTicks, error) {
	total := core.KnownCost(0)
	for _, model := range models {
		sum, err := core.AddCost(total, model.CostUsdTicks)
		if err != nil {
			return core.CostUsdTicks{}, errors.New(overflow)
		}
		total = sum
	}
	return total, nil
}

// usageAgentRoles is UsageAgentRole in declaration order, which its Ord
// follows.
var usageAgentRoles = []core.UsageAgentRole{
	core.UsageAgentRolePrimary,
	core.UsageAgentRoleOrchestrator,
	core.UsageAgentRoleSubtle,
	core.UsageAgentRoleStraightforward,
	core.UsageAgentRoleOracle,
	core.UsageAgentRoleReviewer,
	core.UsageAgentRoleSubagent,
	core.UsageAgentRoleUnclassifiedSubagent,
}

func loadWith(path string, query *core.SessionUsageQuery, nowMs int64, catalogs core.ModelCatalogs) (core.SessionUsageReport, error) {
	connection, err := openReader(path)
	if err != nil {
		return core.SessionUsageReport{}, err
	}
	defer connection.Close()
	tx, err := connection.Begin()
	if err != nil {
		return core.SessionUsageReport{}, err
	}
	defer tx.Rollback()
	// The graph always spans the same hours; the rows cover one of them when
	// the query names it.
	graphStart := core.GraphStartMs(nowMs)
	sinceMs, untilMs := graphStart, nowMs
	if query.HourStartMs != nil {
		hour := core.HourStartMs(*query.HourStartMs)
		end := hour + (core.HourMs - 1)
		if hour > math.MaxInt64-(core.HourMs-1) {
			end = math.MaxInt64
		}
		sinceMs, untilMs = hour, min(nowMs, end)
	}
	hours := make([]core.HourlyUsage, core.UsageGraphHours)
	for n := range hours {
		hours[n] = core.HourlyUsage{StartMs: graphStart + int64(n)*core.HourMs}
	}
	report := core.SessionUsageReport{
		SinceMs:      sinceMs,
		UntilMs:      untilMs,
		Profiles:     []string{},
		Sessions:     []core.SessionUsageEstimate{},
		AgentUsage:   []core.AgentUsageEstimate{},
		CostUsdTicks: core.KnownCost(0),
	}
	type root struct {
		id, name, profile, state string
	}
	var roots []root
	err = func() error {
		rows, err := tx.Query(`SELECT session_id, title, session_title_override, acp_session_title, last_profile, state
        FROM sessions s WHERE NOT EXISTS(SELECT 1 FROM subagent_accounting a WHERE a.child_session_id=s.session_id) ORDER BY session_id`)
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			var id, title, profile, state string
			var titleOverride, acpTitle *string
			if err := rows.Scan(&id, &title, &titleOverride, &acpTitle, &profile, &state); err != nil {
				return err
			}
			// Named as the session list names it.
			roots = append(roots, root{
				id:      id,
				name:    core.ListedSessionTitle(id, title, titleOverride, acpTitle),
				profile: profile,
				state:   state,
			})
		}
		return rows.Err()
	}()
	if err != nil {
		return core.SessionUsageReport{}, err
	}
	profiles := map[string]struct{}{}
	for _, root := range roots {
		profiles[root.profile] = struct{}{}
	}
	turnProfiles, err := committedStrings(tx, "SELECT DISTINCT profile FROM session_turn_selections WHERE profile IS NOT NULL")
	if err != nil {
		return core.SessionUsageReport{}, err
	}
	for _, profile := range turnProfiles {
		profiles[profile] = struct{}{}
	}
	report.Profiles = slices.Sorted(maps.Keys(profiles))
	wanted := query.Profile
	type reportEntry struct {
		estimate core.SessionUsageEstimate
		models   usageModelGroups
	}
	var entries []reportEntry
	// One owner per retained member, decided from the same snapshot as
	// usage.
	owners := map[string]int{}
	for _, root := range roots {
		state, ok := core.SessionStateFromStored(root.state)
		if !ok {
			return core.SessionUsageReport{}, errors.New("unknown usage session state")
		}
		suspended := !state.IsActive()
		if suspended && !query.IncludeSuspended {
			continue
		}
		members, err := committedStrings(tx, `WITH RECURSIVE tree(id) AS (
        SELECT ?1 UNION SELECT a.child_session_id FROM subagent_accounting a JOIN tree t ON a.parent_session_id=t.id)
        SELECT id FROM tree`, root.id)
		if err != nil {
			return core.SessionUsageReport{}, err
		}
		index := len(entries)
		for _, member := range members {
			owners[member] = index
		}
		entries = append(entries, reportEntry{
			estimate: core.SessionUsageEstimate{
				SessionID:     root.id,
				Title:         root.name,
				Profile:       root.profile,
				Suspended:     suspended,
				SubagentCount: max(len(members)-1, 0),
				Models:        []core.ModelUsageEstimate{},
				CostUsdTicks:  core.KnownCost(0),
			},
			models: usageModelGroups{},
		})
	}
	roleGroups := map[core.UsageAgentRole]usageModelGroups{
		core.UsageAgentRolePrimary:  {},
		core.UsageAgentRoleSubagent: {},
		core.UsageAgentRoleReviewer: {},
	}
	type turnRow struct {
		member      string
		body        string
		selection   core.UsageSelection
		turnProfile *string
		category    *string
		kind        *string
	}
	var turnRows []turnRow
	err = func() error {
		rows, err := tx.Query(reportTurnsSQL, min(sinceMs, graphStart), nowMs, wanted)
		if err != nil {
			return err
		}
		defer rows.Close()
		for rows.Next() {
			var row turnRow
			if err := rows.Scan(&row.member, &row.body, &row.selection.Model, &row.selection.Effort,
				&row.turnProfile, &row.category, &row.kind); err != nil {
				return err
			}
			turnRows = append(turnRows, row)
		}
		return rows.Err()
	}()
	if err != nil {
		return core.SessionUsageReport{}, err
	}
	for _, row := range turnRows {
		index, ok := owners[row.member]
		if !ok {
			continue
		}
		entry := &entries[index]
		turn, err := deserializeTurnOutcome(row.body)
		if err != nil {
			return core.SessionUsageReport{}, err
		}
		inRows := sinceMs <= turn.CompletedAtMs && turn.CompletedAtMs <= untilMs
		bar := -1
		if offset := turn.CompletedAtMs - graphStart; offset >= 0 {
			if hour := offset / core.HourMs; hour < int64(len(hours)) {
				bar = int(hour)
			}
		}
		if inRows {
			entry.estimate.Coverage.RecordedTurns++
		}
		usage := turn.Usage
		switch {
		case usage != nil && usage.Scope == core.UsageScopeTurn:
			if inRows {
				entry.estimate.Coverage.FullTurnReports++
			}
			var role *core.UsageAgentRole
			if inRows {
				value := core.UsageAgentRolePrimary
				if row.member != entry.estimate.SessionID {
					if value, err = childUsageRole(row.member, row.category, row.kind); err != nil {
						return core.SessionUsageReport{}, err
					}
				}
				role = &value
			}
			var attributed usageGroups
			if err := attribute(usage, row.selection, &attributed); err != nil {
				return core.SessionUsageReport{}, err
			}
			for _, group := range attributed {
				selection := group.selection
				if selection.Model != nil {
					priced := catalogs.PricedModel(row.turnProfile, *selection.Model)
					selection.Model = &priced
				}
				cost, err := estimateModelCost(usage, selection.Model, group.totals)
				if err != nil {
					return core.SessionUsageReport{}, err
				}
				if bar >= 0 {
					hour := &hours[bar]
					if cost != nil {
						if hour.KnownCostUsdTicks > math.MaxUint64-*cost {
							return core.SessionUsageReport{}, errors.New("hour cost overflow")
						}
						hour.KnownCostUsdTicks += *cost
					} else {
						hour.HasUnknownCost = true
					}
				}
				if role == nil {
					continue
				}
				roleGroup := roleGroups[*role]
				if roleGroup == nil {
					roleGroup = usageModelGroups{}
					roleGroups[*role] = roleGroup
				}
				for _, groups := range []usageModelGroups{entry.models, roleGroup} {
					if err := groups.add(selection.Model, cost, group.totals); err != nil {
						return core.SessionUsageReport{}, err
					}
				}
			}
		case usage != nil && usage.Scope == core.UsageScopeLastRequest:
			if inRows {
				entry.estimate.Coverage.LastRequestReports++
			}
		case usage != nil:
			if inRows {
				entry.estimate.Coverage.UnspecifiedReports++
			}
		default:
			if inRows {
				entry.estimate.Coverage.MissingReports++
			}
		}
	}
	// A turn already running at the start of the window can still consume
	// in it.
	active, err := committedStrings(tx, reportActiveSQL, untilMs, wanted)
	if err != nil {
		return core.SessionUsageReport{}, err
	}
	for _, member := range active {
		if index, ok := owners[member]; ok {
			entries[index].estimate.Coverage.UnfinishedTurns++
		}
	}
	report.Hours = hours
	for _, entry := range entries {
		estimate := entry.estimate
		// A profile selects its own parents and any parent whose tree ran a
		// turn on it in the period; only that profile's turns are counted.
		if wanted != nil && *wanted != estimate.Profile &&
			estimate.Coverage.RecordedTurns == 0 && estimate.Coverage.UnfinishedTurns == 0 {
			continue
		}
		estimate.Models = entry.models.sorted()
		if estimate.CostUsdTicks, err = usageSumCosts(estimate.Models, "session cost overflow"); err != nil {
			return core.SessionUsageReport{}, err
		}
		if report.CostUsdTicks, err = core.AddCost(report.CostUsdTicks, estimate.CostUsdTicks); err != nil {
			return core.SessionUsageReport{}, errors.New("report cost overflow")
		}
		report.Sessions = append(report.Sessions, estimate)
	}
	slices.SortStableFunc(report.Sessions, func(a, b core.SessionUsageEstimate) int {
		if order := core.DescendingCost(a.CostUsdTicks, b.CostUsdTicks); order != 0 {
			return order
		}
		return strings.Compare(a.SessionID, b.SessionID)
	})
	for _, role := range usageAgentRoles {
		groups, ok := roleGroups[role]
		if !ok {
			continue
		}
		models := groups.sorted()
		cost, err := usageSumCosts(models, "agent role cost overflow")
		if err != nil {
			return core.SessionUsageReport{}, err
		}
		report.AgentUsage = append(report.AgentUsage, core.AgentUsageEstimate{
			Role:         role,
			Models:       models,
			CostUsdTicks: cost,
		})
	}
	return report, nil
}

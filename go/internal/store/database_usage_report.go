// Port of mj-store/src/database/usage_report.rs.

// One consistent store snapshot of parent usage and retained descendants.

package store

import (
	"fmt"
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

func loadWith(path string, query *core.SessionUsageQuery, nowMs int64, catalogs core.ModelCatalogs) (core.SessionUsageReport, error) {
	panic("unported: mj-store/src/database/usage_report.rs load_with")
}

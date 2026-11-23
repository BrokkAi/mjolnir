// Port of mj-core/src/usage/report.rs.
//
// Read-only session accounting report shared by the daemon and terminal.

package core

import (
	"encoding/json"
	"errors"
	"math"
)

const HourMs int64 = 3_600_000

// UsageGraphHours is how many whole clock hours the graph shows, ending with
// the current one.
const UsageGraphHours int64 = 24

// HourStartMs is the start of the clock hour (UTC) that contains ms.
func HourStartMs(ms int64) int64 {
	// An API caller can send any i64; the extremes saturate.
	hours := ms / HourMs
	if ms%HourMs < 0 {
		hours--
	}
	if hours > math.MaxInt64/HourMs {
		return math.MaxInt64
	}
	if hours < math.MinInt64/HourMs {
		return math.MinInt64
	}
	return hours * HourMs
}

// GraphStartMs is the start of the oldest hour the graph shows when nowMs is
// the current time.
func GraphStartMs(nowMs int64) int64 {
	return HourStartMs(nowMs) - (UsageGraphHours-1)*HourMs
}

// SessionUsageQuery ignores unknown fields: a client from before the hour
// graph still sends period, and the report it gets back is the graph's 24
// hours.
type SessionUsageQuery struct {
	// Profile counts only turns recorded on this profile, anywhere in a
	// parent's sub-agent tree; nil counts every turn. Rows remain whole
	// parents.
	Profile *string `json:"profile"`
	// HourStartMs is the start of one clock hour (see HourStartMs) whose
	// turns the rows cover. Nil covers every hour the graph shows.
	HourStartMs      *int64 `json:"hour_start_ms"`
	IncludeSuspended bool   `json:"include_suspended" serde:"default"`
}

func (q *SessionUsageQuery) UnmarshalJSON(data []byte) error {
	type plain SessionUsageQuery
	return transcriptStrictDecode(data, (*plain)(q), false)
}

// HourlyUsage is one bar of the usage graph: the turns that completed in one
// clock hour.
type HourlyUsage struct {
	StartMs int64 `json:"start_ms"`
	// KnownCostUsdTicks is the sum of the costs that are known.
	KnownCostUsdTicks uint64 `json:"known_cost_usd_ticks"`
	// HasUnknownCost: some turn in the hour used a model that cannot be
	// priced, so the known sum is a lower bound.
	HasUnknownCost bool `json:"has_unknown_cost"`
}

func (h *HourlyUsage) UnmarshalJSON(data []byte) error {
	type plain HourlyUsage
	return transcriptStrictDecode(data, (*plain)(h), false)
}

// UnknownCost marks recorded tokens that cannot be priced: the model has no
// known rate, or its counters do not say how many tokens of each kind it
// used. It serializes as null, like Rust's unit struct.
type UnknownCost struct{}

func (UnknownCost) MarshalJSON() ([]byte, error) { return []byte("null"), nil }

// CostUsdTicks is a token cost in 10^-10 USD, or why it cannot be known
// (Rust Result<u64, UnknownCost>). The zero value is a known cost of 0.
// Serialized as serde writes a Result: {"Ok":n} or {"Err":null}.
type CostUsdTicks struct {
	Ticks   uint64
	Unknown bool
}

func KnownCost(ticks uint64) CostUsdTicks { return CostUsdTicks{Ticks: ticks} }

func (c CostUsdTicks) MarshalJSON() ([]byte, error) {
	if c.Unknown {
		return transcriptMarshalMembers([]transcriptMember{{"Err", UnknownCost{}}})
	}
	return transcriptMarshalMembers([]transcriptMember{{"Ok", c.Ticks}})
}

func (c *CostUsdTicks) UnmarshalJSON(data []byte) error {
	members, err := transcriptObject(data)
	if err != nil {
		return err
	}
	if len(members) != 1 {
		return errors.New("invalid type: map, expected map with a single key")
	}
	if raw, ok := members["Ok"]; ok {
		var ticks uint64
		if err := json.Unmarshal(raw, &ticks); err != nil {
			return err
		}
		*c = CostUsdTicks{Ticks: ticks}
		return nil
	}
	if raw, ok := members["Err"]; ok {
		if string(raw) != "null" {
			return errors.New("invalid type, expected unit struct UnknownCost")
		}
		*c = CostUsdTicks{Unknown: true}
		return nil
	}
	for tag := range members {
		return transcriptUnknownVariant(tag, "Ok", "Err")
	}
	return nil
}

// AddCost adds cost to total; an unknown part makes the whole sum unknown.
func AddCost(total, cost CostUsdTicks) (CostUsdTicks, error) {
	if total.Unknown || cost.Unknown {
		return CostUsdTicks{Unknown: true}, nil
	}
	if total.Ticks > math.MaxUint64-cost.Ticks {
		return CostUsdTicks{}, errors.New("cost overflow")
	}
	return CostUsdTicks{Ticks: total.Ticks + cost.Ticks}, nil
}

// DescendingCost orders costs from highest to lowest, with unknown costs
// last. It returns a negative number when a sorts first.
func DescendingCost(a, b CostUsdTicks) int {
	switch {
	case a.Unknown && b.Unknown:
		return 0
	case a.Unknown:
		return 1
	case b.Unknown:
		return -1
	case a.Ticks > b.Ticks:
		return -1
	case a.Ticks < b.Ticks:
		return 1
	}
	return 0
}

type ModelUsageEstimate struct {
	Model        *string                      `json:"model"`
	Totals       map[string]UsageCounterTotal `json:"totals"`
	CostUsdTicks CostUsdTicks                 `json:"cost_usd_ticks"`
}

func (e *ModelUsageEstimate) UnmarshalJSON(data []byte) error {
	type plain ModelUsageEstimate
	return transcriptStrictDecode(data, (*plain)(e), false)
}

type SessionUsageEstimate struct {
	SessionID     string               `json:"session_id"`
	Title         string               `json:"title"`
	Profile       string               `json:"profile"`
	Suspended     bool                 `json:"suspended"`
	SubagentCount int                  `json:"subagent_count"`
	Models        []ModelUsageEstimate `json:"models"`
	Coverage      UsageCoverage        `json:"coverage"`
	// CostUsdTicks is the sum of Models; unknown when any model's cost is
	// unknown.
	CostUsdTicks CostUsdTicks `json:"cost_usd_ticks"`
}

func (e *SessionUsageEstimate) UnmarshalJSON(data []byte) error {
	type plain SessionUsageEstimate
	return transcriptStrictDecode(data, (*plain)(e), false)
}

// IsPartial: some turns in the period are still running or reported no
// full usage, so a known cost is a lower bound.
func (e *SessionUsageEstimate) IsPartial() bool {
	return e.Coverage.FullTurnReports < e.Coverage.RecordedTurns || e.Coverage.UnfinishedTurns > 0
}

type UsageAgentRole string

const (
	UsageAgentRolePrimary              UsageAgentRole = "primary"
	UsageAgentRoleOrchestrator         UsageAgentRole = "orchestrator"
	UsageAgentRoleSubtle               UsageAgentRole = "subtle"
	UsageAgentRoleStraightforward      UsageAgentRole = "straightforward"
	UsageAgentRoleOracle               UsageAgentRole = "oracle"
	UsageAgentRoleReviewer             UsageAgentRole = "reviewer"
	UsageAgentRoleSubagent             UsageAgentRole = "subagent"
	UsageAgentRoleUnclassifiedSubagent UsageAgentRole = "unclassified_subagent"
)

func (r UsageAgentRole) Label() string {
	switch r {
	case UsageAgentRolePrimary:
		return "Primary agents"
	case UsageAgentRoleOrchestrator:
		return "Orchestrators"
	case UsageAgentRoleSubtle:
		return "Subtle subagents"
	case UsageAgentRoleStraightforward:
		return "Straightforward subagents"
	case UsageAgentRoleOracle:
		return "Oracles"
	case UsageAgentRoleReviewer:
		return "Reviewers"
	case UsageAgentRoleSubagent:
		return "Other subagents"
	}
	return "Unclassified subagents"
}

// usageAgentRoleOrder is the Rust declaration order, which Ord follows.
func (r UsageAgentRole) usageAgentRoleOrder() int {
	switch r {
	case UsageAgentRolePrimary:
		return 0
	case UsageAgentRoleOrchestrator:
		return 1
	case UsageAgentRoleSubtle:
		return 2
	case UsageAgentRoleStraightforward:
		return 3
	case UsageAgentRoleOracle:
		return 4
	case UsageAgentRoleReviewer:
		return 5
	case UsageAgentRoleSubagent:
		return 6
	}
	return 7
}

type AgentUsageEstimate struct {
	Role         UsageAgentRole       `json:"role"`
	Models       []ModelUsageEstimate `json:"models"`
	CostUsdTicks CostUsdTicks         `json:"cost_usd_ticks"`
}

func (e *AgentUsageEstimate) UnmarshalJSON(data []byte) error {
	type plain AgentUsageEstimate
	return transcriptStrictDecode(data, (*plain)(e), false)
}

type SessionUsageReport struct {
	// SinceMs and UntilMs are the span Sessions and AgentUsage cover: the
	// selected hour, or every hour the graph shows.
	SinceMs int64 `json:"since_ms"`
	UntilMs int64 `json:"until_ms"`
	// Hours has one bar per hour the graph shows, oldest first, for the
	// same profile and suspended filters but regardless of the selected hour.
	Hours []HourlyUsage `json:"hours" serde:"default"`
	// Profiles includes retained parent and turn profile names even if no
	// longer configured.
	Profiles []string               `json:"profiles"`
	Sessions []SessionUsageEstimate `json:"sessions"`
	// AgentUsage is filtered model totals grouped by each session's
	// retained role. Children destroyed before kind retention was introduced
	// remain unclassified.
	AgentUsage []AgentUsageEstimate `json:"agent_usage" serde:"default"`
	// CostUsdTicks is the sum of Sessions; unknown when any session's cost
	// is unknown.
	CostUsdTicks CostUsdTicks `json:"cost_usd_ticks"`
}

func (r *SessionUsageReport) UnmarshalJSON(data []byte) error {
	type plain SessionUsageReport
	return transcriptStrictDecode(data, (*plain)(r), false)
}

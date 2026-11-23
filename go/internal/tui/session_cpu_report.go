// Port of mj-tui/src/session_cpu_report.rs.
//
// A live, selectable CPU report grouped by machine.
//
// A parent's row shows its tree total (itself plus its listed sub-agents) and
// its accounting scope. Listed sub-agents appear below their parent.

package tui

import (
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// CpuMetric is which CPU figure the report shows, sorts by and totals.
type CpuMetric uint8

const (
	CpuMetricHourly CpuMetric = iota
	CpuMetricRecent
)

func (m CpuMetric) label() string {
	if m == CpuMetricRecent {
		return "recent"
	}
	return "hourly"
}

// cpuMetricPick is Rust's CpuMetric::pick: hourly or recent, by metric.
func cpuMetricPick[T any](metric CpuMetric, hourly T, recent T) T {
	if metric == CpuMetricRecent {
		return recent
	}
	return hourly
}

// MachineReport is one machine's tab of the CPU report.
type MachineReport struct {
	key    string
	metric CpuMetric
	// totalPermille is the machine's total of metric.
	totalPermille uint32
	partial       bool
	// lines are the display rows, including blank root separators. rowMap is
	// nil for those separators and maps session rows to sessionIDs
	// otherwise.
	lines      []chat.Line
	rowMap     []*int
	sessionIDs []string
}

func (r *MachineReport) tabLabel() string {
	partial := ""
	if r.partial {
		partial = "+"
	}
	return fmt.Sprintf("%s · %s%s %s", r.key,
		client.FormatCPUPermille(clampPermille(r.totalPermille)), partial, r.metric.label())
}

// reportGroups groups and orders the report. Machine keys are sorted by name
// so CPU changes update totals without moving tabs around.
func reportGroups(dashboard *DashboardState, metric CpuMetric) []MachineReport {
	panic("unported: mj-tui/src/session_cpu_report.rs report_groups")
}

// treePermille is a session's tree total of metric: itself plus its live
// descendants.
func treePermille(dashboard *DashboardState, sessionID string, metric CpuMetric) uint32 {
	panic("unported: mj-tui/src/session_cpu_report.rs tree_permille")
}

// reportLines is all groups as plain lines, used to detect live report
// changes even when a different machine tab is currently visible. It keeps
// the unprefixed name: setup/report.rs's report_lines sorts after this
// module and is the one prefixed.
func reportLines(dashboard *DashboardState, metric CpuMetric) []chat.Line {
	panic("unported: mj-tui/src/session_cpu_report.rs report_lines")
}

// CpuSummary is how one session's CPU reads, in the report and in the
// session menu.
type CpuSummary struct {
	main string
	// muted is true when main is a statement about missing data, not a
	// figure.
	muted bool
	// detail is context for main: tree membership, own share and coverage
	// for a parent, or the covered period of a short history. Nil for none.
	detail *string
	// tree is true for the tree breakdown, which is too long to share a
	// line.
	tree bool
}

func (s *CpuSummary) mainSpan() chat.Span {
	if s.muted {
		return chat.StyledSpan(s.main, chat.Muted())
	}
	return chat.RawSpan(s.main)
}

// cpuSummary is the one wording of a session's CPU, read from the rollup
// owner.
func cpuSummary(dashboard *DashboardState, sessionID string) CpuSummary {
	return cpuSummaryOf(dashboard, sessionID, nil)
}

// cpuSummaryOf is cpuSummary with only metric's figure, or both figures for
// nil.
func cpuSummaryOf(dashboard *DashboardState, sessionID string, metric *CpuMetric) CpuSummary {
	panic("unported: mj-tui/src/session_cpu_report.rs cpu_summary_of")
}

// pushTree adds one session's selectable line, then its listed sub-agents
// indented below.
func pushTree(dashboard *DashboardState, session *core.SessionRecord, listed map[string]struct{}, depth int, report *MachineReport) {
	panic("unported: mj-tui/src/session_cpu_report.rs push_tree")
}

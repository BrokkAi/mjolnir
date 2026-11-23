// Port of mj-tui/src/session_cpu_report.rs.
//
// A live, selectable CPU report grouped by machine.
//
// A parent's row shows its tree total (itself plus its listed sub-agents) and
// its accounting scope. Listed sub-agents appear below their parent.

package tui

import (
	"cmp"
	"fmt"
	"maps"
	"slices"
	"strings"

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
	groups := map[string][]*core.SessionRecord{}
	capacityIDs := slices.Sorted(maps.Keys(dashboard.capacityDetails))
	for session := range dashboard.state.Get().Sessions.Values() {
		if !session.State.IsActive() || session.State == core.SessionStateParked ||
			session.State == core.SessionStateError {
			continue
		}
		machine := ""
		found := false
		for _, id := range capacityIDs {
			reading := dashboard.capacityDetails[id].Reading
			if slices.Contains(reading.TargetIDs, session.TargetTemplateID) {
				machine, found = reading.Host, true
				break
			}
		}
		if !found {
			machine = sessionTargetLabel(dashboard.state.Get(), session, nil, dashboard.config.Get())
		}
		groups[machine] = append(groups[machine], session)
	}

	treeShare := func(session *core.SessionRecord) uint32 { return treePermille(dashboard, session.ID, metric) }

	reports := make([]MachineReport, 0, len(groups))
	for _, key := range slices.Sorted(maps.Keys(groups)) {
		sessions := groups[key]
		ids := make([]string, 0, len(sessions))
		for _, session := range sessions {
			ids = append(ids, session.ID)
		}
		total := dashboard.cpuTotal(ids)
		totalPermille := cpuMetricPick(metric, total.hourlyPermille, total.recentPermille)
		// Nest only within the same machine; moved children retain their own tab.
		local := map[string]struct{}{}
		for _, id := range ids {
			local[id] = struct{}{}
		}
		nested := map[string]struct{}{}
		for _, id := range rowIndexSortedKeys(local) {
			for _, child := range dashboard.cpuChildren(id) {
				if _, ok := local[child]; ok {
					nested[child] = struct{}{}
				}
			}
		}
		sessions = slices.DeleteFunc(sessions, func(session *core.SessionRecord) bool {
			_, ok := nested[session.ID]
			return ok
		})
		slices.SortStableFunc(sessions, func(a, b *core.SessionRecord) int {
			if c := cmp.Compare(treeShare(b), treeShare(a)); c != 0 {
				return c
			}
			return cmp.Compare(a.ID, b.ID)
		})

		report := MachineReport{
			key:           key,
			metric:        metric,
			totalPermille: totalPermille,
			partial:       total.isPartial(),
		}
		for index, session := range sessions {
			if index > 0 {
				report.lines = append(report.lines, chat.Line{})
				report.rowMap = append(report.rowMap, nil)
			}
			pushTree(dashboard, session, local, 1, &report)
		}
		reports = append(reports, report)
	}
	return reports
}

// treePermille is a session's tree total of metric: itself plus its live
// descendants.
func treePermille(dashboard *DashboardState, sessionID string, metric CpuMetric) uint32 {
	rollup := dashboard.cpuRollup(sessionID)
	return cpuMetricPick(metric, rollup.hourlyPermille, rollup.recentPermille)
}

// reportLines is all groups as plain lines, used to detect live report
// changes even when a different machine tab is currently visible. It keeps
// the unprefixed name: setup/report.rs's report_lines sorts after this
// module and is the one prefixed.
func reportLines(dashboard *DashboardState, metric CpuMetric) []chat.Line {
	groups := reportGroups(dashboard, metric)
	var lines []chat.Line
	if len(groups) > 0 {
		lines = append(lines, chat.StyledLine(fmt.Sprintf("Machine CPU share · %s", metric.label()), chat.Muted()))
	}
	for index, group := range groups {
		if index > 0 {
			lines = append(lines, chat.Line{})
		}
		partial := ""
		if group.partial {
			partial = "+"
		}
		lines = append(lines, chat.StyledLine(
			fmt.Sprintf("%s  %s%s %s", group.key,
				client.FormatCPUPermille(clampPermille(group.totalPermille)), partial, metric.label()),
			chat.Style{}.Foreground(chat.ThemePalette().Text).Bold(),
		))
		lines = append(lines, group.lines...)
	}
	return lines
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
	if owner, ok := dashboard.cpuIncludedIn(sessionID); ok {
		var detail *string
		if session := dashboard.state.Get().Sessions.GetShared(owner); session != nil {
			name := sessionName(session)
			detail = &name
		}
		return CpuSummary{main: "included in parent", muted: true, detail: detail}
	}
	rollup := dashboard.cpuRollup(sessionID)
	var own *client.SessionCpuView
	if usage, ok := dashboard.cpuSample(sessionID); ok {
		own = &client.SessionCpuView{State: client.SessionCpuViewMeasured, Usage: usage}
	} else {
		own = dashboard.sessionCPU.Get().GetShared(sessionID)
	}
	scopeLabel := func(scope core.CpuSampleScope) string {
		if scope == core.CpuSampleScopeContainer {
			return "container"
		}
		return "process tree"
	}
	if rollup.hasDescendants() && rollup.measured > 0 {
		marker := ""
		if rollup.isPartial() {
			marker = "+"
		}
		ownText := "own unmeasured"
		if own != nil && own.State == client.SessionCpuViewMeasured {
			figure := CpuMetricRecent
			if metric != nil {
				figure = *metric
			}
			ownText = fmt.Sprintf("%s %s", scopeLabel(own.Usage.Scope),
				client.FormatCPUPermille(cpuMetricPick(figure, own.Usage.HourlyPermille, own.Usage.RecentPermille)))
		}
		measured := ""
		if rollup.isPartial() {
			measured = fmt.Sprintf(", %d of %d measured", rollup.measured, rollup.members)
		}
		var main string
		if metric != nil {
			main = fmt.Sprintf("%s%s %s",
				client.FormatCPUPermille(clampPermille(treePermille(dashboard, sessionID, *metric))),
				marker, metric.label())
		} else {
			main = fmt.Sprintf("%s%s hourly / %s%s recent",
				client.FormatCPUPermille(clampPermille(rollup.hourlyPermille)), marker,
				client.FormatCPUPermille(clampPermille(rollup.recentPermille)), marker)
		}
		detail := fmt.Sprintf("tree of %d; %s%s", rollup.members, ownText, measured)
		return CpuSummary{main: main, detail: &detail, tree: true}
	}
	switch {
	case own != nil && own.State == client.SessionCpuViewMeasured:
		usage := own.Usage
		// The covered period qualifies only the hourly average.
		seconds := usage.HourlyCoveredSecs
		var period *string
		if seconds < 3600 && (metric == nil || *metric != CpuMetricRecent) {
			text := fmt.Sprintf("%dm", seconds/60)
			if seconds < 60 {
				text = fmt.Sprintf("%ds", seconds)
			}
			period = &text
		}
		detail := scopeLabel(usage.Scope)
		if period != nil {
			detail = fmt.Sprintf("%s; %s", detail, *period)
		}
		var main string
		if metric != nil {
			main = fmt.Sprintf("%s %s",
				client.FormatCPUPermille(cpuMetricPick(*metric, usage.HourlyPermille, usage.RecentPermille)),
				metric.label())
		} else {
			main = fmt.Sprintf("%s hourly / %s recent",
				client.FormatCPUPermille(usage.HourlyPermille), client.FormatCPUPermille(usage.RecentPermille))
		}
		return CpuSummary{main: main, detail: &detail}
	case own != nil:
		return CpuSummary{main: fmt.Sprintf("CPU unavailable: %s", own.Reason), muted: true}
	default:
		return CpuSummary{main: "no CPU data yet", muted: true}
	}
}

// pushTree adds one session's selectable line, then its listed sub-agents
// indented below.
func pushTree(dashboard *DashboardState, session *core.SessionRecord, listed map[string]struct{}, depth int, report *MachineReport) {
	metric := report.metric
	summary := cpuSummaryOf(dashboard, session.ID, &metric)
	spans := []chat.Span{
		chat.RawSpan(fmt.Sprintf("%s%s  [%s]  ", strings.Repeat("  ", depth), sessionName(session), session.LastProfile)),
		summary.mainSpan(),
	}
	if summary.detail != nil {
		spans = append(spans, chat.StyledSpan(fmt.Sprintf(" (%s)", *summary.detail), chat.Muted()))
	}
	rowIndex := len(report.sessionIDs)
	report.sessionIDs = append(report.sessionIDs, session.ID)
	report.lines = append(report.lines, chat.LineFrom(spans...))
	report.rowMap = append(report.rowMap, &rowIndex)

	var children []*core.SessionRecord
	for _, child := range dashboard.cpuChildren(session.ID) {
		if _, ok := listed[child]; !ok {
			continue
		}
		if record := dashboard.state.Get().Sessions.GetShared(child); record != nil {
			children = append(children, record)
		}
	}
	subtreeShare := func(record *core.SessionRecord) uint32 { return treePermille(dashboard, record.ID, report.metric) }
	slices.SortStableFunc(children, func(a, b *core.SessionRecord) int {
		if c := cmp.Compare(subtreeShare(b), subtreeShare(a)); c != 0 {
			return c
		}
		return cmp.Compare(a.ID, b.ID)
	})
	for _, child := range children {
		pushTree(dashboard, child, listed, depth+1, report)
	}
}

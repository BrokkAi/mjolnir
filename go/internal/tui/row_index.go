// Port of mj-tui/src/row_index.rs.
//
// Incremental membership for ordinary rendering; the resume dialog lists
// stopped records itself. Direct presentation edits use the same input map.
//
// SessionFacts synchronizes the index when the records it is built from
// change; readers go through the facts first.

package tui

import (
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// RowIndex is incremental session-list membership.
type RowIndex struct {
	records   core.SnapshotMap[string, core.SessionRecord]
	relations core.SnapshotMap[string, core.SubagentMetadata]
	live      map[string]struct{}
	children  map[string]map[string]struct{}
	// activeChildren are the children with a live worker, by parent.
	activeChildren map[string]map[string]struct{}
	// visits counts record changes the index processed, for tests.
	visits int
}

func (r *RowIndex) synchronize(state *core.StateState) {
	panic("unported: mj-tui/src/row_index.rs RowIndex::synchronize")
}

func (d *DashboardState) listedSessionCandidates() []*core.SessionRecord {
	panic("unported: mj-tui/src/row_index.rs DashboardState::listed_session_candidates")
}

// synchronizedRowIndex is the index, synchronized with the current records.
func (d *DashboardState) synchronizedRowIndex() *RowIndex {
	d.sessionFacts()
	return &d.rowIndex
}

func (d *DashboardState) managedChildIDsOfKind(parent string, kind core.SubagentKind) []string {
	panic("unported: mj-tui/src/row_index.rs DashboardState::managed_child_ids_of_kind")
}

func (d *DashboardState) ReviewerCountsFor(parent string) chat.ReviewerCounts {
	reviewers := d.managedChildIDsOfKind(parent, core.SubagentKindReviewer)
	index := d.synchronizedRowIndex()
	open := 0
	for _, id := range reviewers {
		if _, live := index.live[id]; live {
			open++
		}
	}
	return chat.ReviewerCounts{Total: len(reviewers), Open: open}
}

func (d *DashboardState) managedActiveChildIDs(parent string) []string {
	return rowIndexSortedKeys(d.synchronizedRowIndex().activeChildren[parent])
}

func (d *DashboardState) managedChildIDs(parent string) []string {
	return rowIndexSortedKeys(d.synchronizedRowIndex().children[parent])
}

// rowIndexSortedKeys is a BTreeSet<String>'s iteration order: the members sorted.
func rowIndexSortedKeys(set map[string]struct{}) []string {
	keys := make([]string, 0, len(set))
	for key := range set {
		keys = append(keys, key)
	}
	slices.Sort(keys)
	return keys
}

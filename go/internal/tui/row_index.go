// Port of mj-tui/src/row_index.rs.
//
// Incremental membership for ordinary rendering; the resume dialog lists
// stopped records itself. Direct presentation edits use the same input map.
//
// SessionFacts synchronizes the index when the records it is built from
// change; readers go through the facts first.

package tui

import (
	"reflect"
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

// rowIndexRecordEqual is SessionRecord's derived PartialEq.
func rowIndexRecordEqual(a, b *core.SessionRecord) bool { return reflect.DeepEqual(a, b) }

// rowIndexRelationEqual is SubagentMetadata's derived PartialEq.
func rowIndexRelationEqual(a, b *core.SubagentMetadata) bool { return reflect.DeepEqual(a, b) }

// rowIndexSetRemove removes member from the set at key, dropping the set once
// it is empty.
func rowIndexSetRemove(sets map[string]map[string]struct{}, key, member string) {
	members, ok := sets[key]
	if !ok {
		return
	}
	delete(members, member)
	if len(members) == 0 {
		delete(sets, key)
	}
}

// rowIndexSetInsert adds member to the set at key, creating the set.
func rowIndexSetInsert(sets map[string]map[string]struct{}, key, member string) {
	members, ok := sets[key]
	if !ok {
		members = map[string]struct{}{}
		sets[key] = members
	}
	members[member] = struct{}{}
}

func (r *RowIndex) synchronize(state *core.StateState) {
	if r.live == nil {
		r.live = map[string]struct{}{}
	}
	if r.children == nil {
		r.children = map[string]map[string]struct{}{}
	}
	if r.activeChildren == nil {
		r.activeChildren = map[string]map[string]struct{}{}
	}
	changed := map[string]struct{}{}
	for id, record := range r.records.Changes(state.Sessions, rowIndexRecordEqual) {
		r.visits++
		changed[id] = struct{}{}
		delete(r.live, id)
		if record != nil && record.State.IsActive() {
			r.live[id] = struct{}{}
		}
	}
	for id, relation := range r.relations.Changes(state.Subagents, rowIndexRelationEqual) {
		changed[id] = struct{}{}
		if old := r.relations.GetShared(id); old != nil {
			rowIndexSetRemove(r.children, old.ParentSessionID, id)
		}
		if relation != nil {
			rowIndexSetInsert(r.children, relation.ParentSessionID, id)
		}
	}
	for _, id := range rowIndexSortedKeys(changed) {
		if previous := r.relations.GetShared(id); previous != nil {
			rowIndexSetRemove(r.activeChildren, previous.ParentSessionID, id)
		}
		record := state.Sessions.GetShared(id)
		if relation := state.Subagents.GetShared(id); record != nil && record.State.HasLiveWorker() && relation != nil {
			rowIndexSetInsert(r.activeChildren, relation.ParentSessionID, id)
		}
	}
	r.records = state.Sessions
	r.relations = state.Subagents
}

func (d *DashboardState) listedSessionCandidates() []*core.SessionRecord {
	listed := d.sessionFacts().listed()
	candidates := make([]*core.SessionRecord, 0, len(listed))
	for _, id := range listed {
		if record := d.state.Get().Sessions.GetShared(id); record != nil {
			candidates = append(candidates, record)
		}
	}
	return candidates
}

// synchronizedRowIndex is the index, synchronized with the current records.
func (d *DashboardState) synchronizedRowIndex() *RowIndex {
	d.sessionFacts()
	return &d.rowIndex
}

func (d *DashboardState) managedChildIDsOfKind(parent string, kind core.SubagentKind) []string {
	var ids []string
	for _, id := range d.managedChildIDs(parent) {
		relation := d.state.Get().Subagents.GetShared(id)
		// The oracle is listed with the sub-agents.
		if relation != nil && (relation.Kind == kind ||
			(kind == core.SubagentKindWorker && relation.Kind == core.SubagentKindOracle)) {
			ids = append(ids, id)
		}
	}
	return ids
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

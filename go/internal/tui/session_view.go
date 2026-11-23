// Port of mj-tui/src/session_view.rs.

// What the dashboard derives about its sessions: each session's attention
// level, the sessions the Sessions pane can list, and the pane's order,
// project groups, and rows.
//
// Rows, badges, the attention queue, notifications, and key handling all
// read these facts, many times per frame. They are derived once per change
// of the inputs they come from, and every reader asks the dashboard for the
// derived result rather than recomputing it. Each input is a [Tracked]
// field, so any change to it is also what marks the facts stale: there is
// no separate invalidation for a new mutation to forget.

package tui

import (
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// revisionCounter hands out revisions from one counter, so a replaced value
// never repeats a revision an earlier value had.
var revisionCounter atomic.Uint64

// nextRevision is the next unused revision. The first one is 1.
func nextRevision() uint64 {
	return revisionCounter.Add(1)
}

// Tracked is a dashboard input that the session facts are derived from.
//
// Reading goes through Get and costs nothing; callers must not change the
// value through the pointer Get returns. Every change goes through Mut or
// Set, which take a new revision; the derived facts remember the revisions
// they were made from and are rebuilt when one differs. A mutable access
// that changes nothing only costs a rebuild.
//
// The zero value holds the zero T at revision 0, which no later revision
// repeats, so it is a valid starting value (Rust's Default).
type Tracked[T any] struct {
	value    T
	revision uint64
}

// NewTracked tracks value under a fresh revision (Rust's Tracked::new and
// From<T>).
func NewTracked[T any](value T) Tracked[T] {
	return Tracked[T]{value: value, revision: nextRevision()}
}

// Revision is the revision of the value Tracked holds now.
func (t *Tracked[T]) Revision() uint64 {
	return t.revision
}

// Get reads the value (Rust's Deref). It does not take a new revision.
func (t *Tracked[T]) Get() *T {
	return &t.value
}

// Mut gives mutable access to the value (Rust's DerefMut), taking a new
// revision first.
func (t *Tracked[T]) Mut() *T {
	t.revision = nextRevision()
	return &t.value
}

// Set replaces the value under a new revision (`*tracked = value`).
func (t *Tracked[T]) Set(value T) {
	*t.Mut() = value
}

// SessionFacts are facts about each session that do not depend on what the
// pane shows.
type SessionFacts struct {
	// inputs are the revisions of the inputs these facts were derived from;
	// nil before the first derivation.
	inputs *[7]uint64
	// own is served attention with this terminal's unread receipt applied.
	own map[string]AttentionLevel
	// listedIDs are live records, and records an operation keeps on screen,
	// by id.
	listedIDs []string
	// derivations counts how many times the facts were derived, for tests.
	derivations int
}

// deriveSessionFacts is Rust's SessionFacts::derive.
func deriveSessionFacts(dashboard *DashboardState, inputs [7]uint64) SessionFacts {
	panic("unported: mj-tui/src/session_view.rs SessionFacts::derive")
}

func (f *SessionFacts) attentionLevel(sessionID string) AttentionLevel {
	if level, ok := f.own[sessionID]; ok {
		return level
	}
	return AttentionLevelInactive
}

func (f *SessionFacts) listed() []string {
	return f.listedIDs
}

// CpuRollup is a session's CPU together with its sub-agents', for display.
//
// A parent's figure is its own CPU plus that of every descendant that has a
// live worker, so a parent that waits on busy children still shows load.
// Recent and hourly shares are summed (a share can exceed 100% across
// processes). If some member (the session or a live descendant) has no
// measurement, the sum is a lower bound and the rollup is partial.
type CpuRollup struct {
	recentPermille uint32
	hourlyPermille uint32
	// members is the session itself plus its live descendants.
	members  int
	measured int
}

func (r CpuRollup) isPartial() bool {
	return r.measured < r.members
}

func (r CpuRollup) hasDescendants() bool {
	return r.members > 1
}

// CpuShare is the rolled-up figure a row shows.
type CpuShare struct {
	permille uint16
	partial  bool
	included bool
}

// label is the row text: the share, then `+` when it is a lower bound.
func (s CpuShare) label() string {
	if s.included {
		return "included in parent"
	}
	marker := ""
	if s.partial {
		marker = "+"
	}
	return client.FormatCPUPermille(s.permille) + marker
}

func clampPermille(permille uint32) uint16 {
	return uint16(min(permille, uint32(^uint16(0))))
}

// CpuRollups are every session's [CpuRollup] and its live sub-agents,
// derived when a CPU sample or the session tree changes. Rows, the live CPU
// report, and the redraw check all read this one result.
type CpuRollups struct {
	inputs    *[2]uint64
	bySession map[string]CpuRollup
	samples   map[string]core.SessionCpuUsage
	owners    map[string]string
	// children are the live sub-agents of each parent, in id order.
	children    map[string][]string
	derivations int
}

// deriveCpuRollups is Rust's CpuRollups::derive.
func deriveCpuRollups(dashboard *DashboardState, inputs [2]uint64) CpuRollups {
	panic("unported: mj-tui/src/session_view.rs CpuRollups::derive")
}

func (r *CpuRollups) total(members []string) CpuRollup {
	panic("unported: mj-tui/src/session_view.rs CpuRollups::total")
}

// SessionOrder is what the Sessions pane shows, in order.
type SessionOrder struct {
	key *OrderKey
	// orderedIDs are the sessions in orderedSessions() order.
	orderedIDs []string
	positions  map[string]int
	// projects holds each ordered session's project key.
	projects []string
	// groupKeys are the project keys in the order their groups appear.
	groupKeys []string
	drawnRows []SessionsRow
	// hiddenCount is how many sessions the Sessions filter holds back.
	hiddenCount              int
	otherWorkspaceMatchCount int
	// targetLabels holds each ordered session's target label.
	targetLabels []string
	// derivations counts how many times the order was derived, for tests.
	derivations int
}

// OrderKey is what the Sessions order was derived from.
type OrderKey struct {
	inputs   [16]uint64
	selected *string
}

// equal is Rust's derived PartialEq.
func (k OrderKey) equal(other OrderKey) bool {
	if k.inputs != other.inputs || (k.selected == nil) != (other.selected == nil) {
		return false
	}
	return k.selected == nil || *k.selected == *other.selected
}

// deriveSessionOrder is Rust's SessionOrder::derive.
func deriveSessionOrder(dashboard *DashboardState, key OrderKey) SessionOrder {
	panic("unported: mj-tui/src/session_view.rs SessionOrder::derive")
}

func (o *SessionOrder) ids() []string {
	return o.orderedIDs
}

func (o *SessionOrder) position(sessionID string) (int, bool) {
	position, ok := o.positions[sessionID]
	return position, ok
}

func (o *SessionOrder) project(index int) (string, bool) {
	if index < 0 || index >= len(o.projects) {
		return "", false
	}
	return o.projects[index], true
}

func (o *SessionOrder) projectKeys() []string {
	return o.groupKeys
}

func (o *SessionOrder) rows() []SessionsRow {
	return o.drawnRows
}

func (o *SessionOrder) hidden() int {
	return o.hiddenCount
}

func (o *SessionOrder) otherWorkspaceMatches() int {
	return o.otherWorkspaceMatchCount
}

func (o *SessionOrder) targets() []string {
	return o.targetLabels
}

func (d *DashboardState) sessionFactsInputs() [7]uint64 {
	return [7]uint64{
		d.state.Revision(),
		d.sessionDetails.Revision(),
		d.sessionReviewsField.Revision(),
		d.servedAttention.Revision(),
		d.notificationDismissals.Revision(),
		d.sessionOperations.Revision(),
		d.sessionVerdicts.Revision(),
	}
}

// sessionFacts are the session facts for the dashboard's current inputs,
// derived again only when one of those inputs changed since the last read.
//
// DashboardState's own storage for them must not be named sessionFacts in
// Go: the method has the name.
func (d *DashboardState) sessionFacts() *SessionFacts {
	panic("unported: mj-tui/src/session_view.rs DashboardState::session_facts")
}

func (d *DashboardState) cpuRollupsInputs() [2]uint64 {
	return [2]uint64{d.state.Revision(), d.sessionCPU.Revision()}
}

// cpuRollups derives the rollups again only when an input changed. The
// storage field must not be named cpuRollups in Go.
func (d *DashboardState) cpuRollups() *CpuRollups {
	panic("unported: mj-tui/src/session_view.rs DashboardState::cpu_rollups")
}

// cpuRollup is a session's CPU with its live sub-agents' included.
func (d *DashboardState) cpuRollup(sessionID string) CpuRollup {
	return d.cpuRollups().bySession[sessionID]
}

func (d *DashboardState) cpuSample(sessionID string) (core.SessionCpuUsage, bool) {
	rollups := d.cpuRollups()
	owner, ok := rollups.owners[sessionID]
	if !ok {
		owner = sessionID
	}
	sample, ok := rollups.samples[owner]
	return sample, ok
}

func (d *DashboardState) cpuIncludedIn(sessionID string) (string, bool) {
	owner, ok := d.cpuRollups().owners[sessionID]
	if !ok || owner == sessionID {
		return "", false
	}
	return owner, true
}

func (d *DashboardState) cpuTotal(ids []string) CpuRollup {
	return d.cpuRollups().total(ids)
}

// cpuShare is the figure a Sessions row shows: absent when nothing in the
// tree is measured. The row applies its own display threshold.
func (d *DashboardState) cpuShare(sessionID string) (CpuShare, bool) {
	rollup := d.cpuRollup(sessionID)
	if rollup.measured == 0 {
		return CpuShare{}, false
	}
	_, included := d.cpuIncludedIn(sessionID)
	return CpuShare{permille: clampPermille(rollup.recentPermille), partial: rollup.isPartial(), included: included}, true
}

// cpuChildren are the live sub-agents of a session, in id order.
func (d *DashboardState) cpuChildren(sessionID string) []string {
	return append([]string(nil), d.cpuRollups().children[sessionID]...)
}

func (d *DashboardState) sessionOrderKey() OrderKey {
	panic("unported: mj-tui/src/session_view.rs DashboardState::session_order_key")
}

// sessionOrder is the Sessions pane's order and rows for the dashboard's
// current inputs and selection, derived again only when one of them changed
// since the last read. The storage field must not be named sessionOrder in
// Go.
func (d *DashboardState) sessionOrder() *SessionOrder {
	panic("unported: mj-tui/src/session_view.rs DashboardState::session_order")
}

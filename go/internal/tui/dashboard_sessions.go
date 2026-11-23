// Port of mj-tui/src/dashboard_sessions.rs.

package tui

import (
	"cmp"
	"fmt"
	"maps"
	"math"
	"slices"
	"strings"
	"time"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// AttentionLevel is how much a session needs a person right now, from least
// to most.
//
// The order is the order a person wants to be interrupted in: a failure
// first, then a session nothing can be learned about because its worker is
// unreachable, then a question the agent cannot proceed without, then an
// important update, then an unread answer, then work in progress, idle, and
// anything stopped or still starting. Rows and priority sorting retain the
// session's status; notification dismissal belongs to the daemon. Values
// compare in that order (Rust's derived Ord).
type AttentionLevel int

const (
	AttentionLevelInactive AttentionLevel = iota
	AttentionLevelIdle
	AttentionLevelWorking
	AttentionLevelUnread
	AttentionLevelImportant
	AttentionLevelWaiting
	AttentionLevelUnreachable
	AttentionLevelFailed
)

// NeedsPerson says whether the attention queue lists a session at this
// level.
func (l AttentionLevel) NeedsPerson() bool {
	return l >= AttentionLevelUnread
}

// AttentionEntry is one session the attention queue would take a person
// to.
type AttentionEntry struct {
	WorkspaceID string
	SessionID   string
	Level       AttentionLevel
}

// PendingNavigation is a session the dashboard is moving to across a
// workspace switch. open says whether arriving opens its conversation or
// only selects its row; the workspace switch and the host both follow it.
type PendingNavigation struct {
	sessionID string
	open      bool
}

// AttentionNavigation keeps workspace preference stable while attention
// navigation changes tabs.
type AttentionNavigation struct {
	sessionID      string
	workspaceID    *string
	queueSessionID string
}

// presentationAttention is the daemon's severity with local unread receipts
// applied. Only SessionFacts calls this; everything else reads the level it
// keeps.
func presentationAttention(dashboard *DashboardState, sessionID string) AttentionLevel {
	served, ok := dashboard.servedAttention.Get().Get(sessionID)
	if !ok {
		served = core.SessionAttentionUnknown
	}
	// The session row, workspace badges, host notifications and notification
	// cards all read this projection. Locally accepted dismissals take effect
	// here before the daemon's next served-attention publication arrives.
	if (served == core.SessionAttentionNeedsInput || served == core.SessionAttentionImportant) &&
		dashboard.notificationDismissalPending(sessionID) {
		return AttentionLevelIdle
	}
	if served == core.SessionAttentionImportant {
		if target, ok := dashboard.notificationDismissals.Get().Get(sessionID); ok {
			if through, ok := target.(core.NotificationDismissalReadThrough); ok {
				if session := dashboard.state.Get().Sessions.GetShared(sessionID); session != nil &&
					session.ViewedThroughEventOrdinal >= through.Value {
					return AttentionLevelIdle
				}
			}
		}
	}
	switch served {
	case core.SessionAttentionNeedsInput:
		return AttentionLevelWaiting
	case core.SessionAttentionImportant:
		return AttentionLevelImportant
	case core.SessionAttentionUnread:
		return AttentionLevelUnread
	case core.SessionAttentionFailed:
		return AttentionLevelFailed
	case core.SessionAttentionUnreachable:
		return AttentionLevelUnreachable
	case core.SessionAttentionWorking:
		return AttentionLevelWorking
	case core.SessionAttentionIdle:
		return AttentionLevelIdle
	default:
		return AttentionLevelInactive
	}
}

// SessionsTextSearch is the daemon's answer to the Sessions filter's text:
// which sessions have it in a user or agent message. The text itself stays
// in SessionsFilter; this holds only what a background search found for it.
// The zero value is Rust's Default.
type SessionsTextSearch struct {
	// requestID is bumped for every search asked for, and for a filter that
	// no longer has text, so an answer to an older search is recognised and
	// dropped.
	requestID uint64
	sync      *SyncEpisode
	// asked is the text last asked about. matches answers it, or an earlier
	// prefix of it while the newer search is out.
	asked string
	// pending: a search is out and has not answered.
	pending *SessionsTextQuery
	// answeredQuery is the query that produced matches, independent of an
	// in-flight refresh.
	answeredQuery string
	matches       map[string]client.SessionTextMatchKind
}

// SessionsTextQuery is why a conversation search is out.
type SessionsTextQuery int

const (
	SessionsTextQueryTyped SessionsTextQuery = iota
	SessionsTextQuerySyncRefresh
)

// isPending says whether a conversation search is out and has not
// answered.
func (s *SessionsTextSearch) isPending() bool {
	return s.pending != nil
}

// CompatibleProfile is one profile a session could move to, with its
// harness.
type CompatibleProfile struct {
	ID   string
	Kind core.HarnessKind
}

// profilePrediction is the sort key load balancing would rank a profile
// by; smaller is preferred.
type profilePrediction struct {
	tier       uint8
	preferRank int
	primary    uint8
	fiveHour   uint8
	id         string
}

func (d *DashboardState) commandSessionID() (string, bool) {
	if d.commandSessionOverride != nil {
		return *d.commandSessionOverride, true
	}
	return d.SelectedSessionID()
}

func (d *DashboardState) commandSession() *core.SessionRecord {
	id, ok := d.commandSessionID()
	if !ok {
		return nil
	}
	return d.state.Get().Sessions.GetShared(id)
}

func (d *DashboardState) selectedSession() *core.SessionRecord {
	return d.commandSession()
}

// visibleSessionIndices is the live sessions the Sessions pane is showing,
// as indices into orderedSessions.
func (d *DashboardState) visibleSessionIndices() []int {
	indices := []int{}
	for _, row := range d.sessionOrder().rows() {
		if session, ok := row.(SessionsRowSession); ok {
			indices = append(indices, session.Index)
		}
	}
	return indices
}

// sessionsRows is the rows the Sessions pane draws at every explicit size:
// a heading per project and one row per live session. (Test-only in Rust.)
//
// Standard and Maximized use four-line session rows; Minimized keeps only
// each session's top summary line. Focus never changes the representation.
func (d *DashboardState) sessionsRows() []SessionsRow {
	return append([]SessionsRow(nil), d.sessionOrder().rows()...)
}

// deriveSessionsRows is the rows for sessions, in order, for sessionOrder to
// keep. numbered says whether more than one project is listed.
func (d *DashboardState) deriveSessionsRows(sessions []*core.SessionRecord, numbered bool) []SessionsRow {
	// Two projects can share a short name, in which case both need their
	// full names to stay distinguishable.
	shortNames := map[string]map[string]struct{}{}
	for _, session := range sessions {
		source := d.ProjectSource(session)
		if shortNames[source.Short] == nil {
			shortNames[source.Short] = map[string]struct{}{}
		}
		shortNames[source.Short][source.Key] = struct{}{}
	}
	grouped := d.config.Get().Advanced.SessionOrder == core.SessionOrderProject && !d.sessionsRankedByMatch()
	collapsed := *d.collapsedProjectKeys.Get()
	rows := []SessionsRow{}
	var previous *string
	number := 0
	for index, session := range sessions {
		source := d.ProjectSource(session)
		if grouped && (previous == nil || *previous != source.Key) {
			number++
			label := source.Short
			if len(shortNames[source.Short]) > 1 {
				label = source.Full
			}
			var shown *int
			if numbered && number <= 9 {
				value := number
				shown = &value
			}
			rows = append(rows, SessionsRowProjectHeading{Key: source.Key, Label: label, Number: shown})
			key := source.Key
			previous = &key
		}
		_, folded := collapsed[source.Key]
		rows = append(rows, SessionsRowSession{Index: index, Expanded: !folded})
	}
	return rows
}

// selectedVisibleIndex is where the selection sits among the rows on
// screen, for the table's highlight. False when nothing is selected or the
// selection is not on screen.
func (d *DashboardState) selectedVisibleIndex() (int, bool) {
	selected, ok := d.SelectedSessionID()
	if !ok {
		return 0, false
	}
	order := d.sessionOrder()
	index, ok := order.position(selected)
	if !ok {
		return 0, false
	}
	visible := 0
	for _, row := range order.rows() {
		if session, ok := row.(SessionsRowSession); ok {
			if session.Index == index {
				return visible, true
			}
			visible++
		}
	}
	return 0, false
}

// orderedSessions is the sessions visible in the selected workspace,
// grouped by project and ordered by creation. Stopped sessions appear only
// among a parent's sub-agents, after the rest; in-flight transitions remain
// visible regardless. The controller may feed all workspaces into one state
// snapshot; the tab is the local view filter.
func (d *DashboardState) orderedSessions() []*core.SessionRecord {
	sessions := []*core.SessionRecord{}
	for _, id := range d.sessionOrder().ids() {
		if session := d.state.Get().Sessions.GetShared(id); session != nil {
			sessions = append(sessions, session)
		}
	}
	return sessions
}

// orderedSessionPosition is where a session is in orderedSessions, if it is
// listed.
func (d *DashboardState) orderedSessionPosition(sessionID string) (int, bool) {
	return d.sessionOrder().position(sessionID)
}

// deriveOrderedSessions is orderedSessions worked out from the inputs, and
// how many sessions the filter holds back, for sessionOrder to keep.
func (d *DashboardState) deriveOrderedSessions() ([]*core.SessionRecord, int) {
	sessions := d.orderedSessionsUnfiltered()
	selected, hasSelected := d.SelectedSessionID()
	if hasSelected {
		if active := d.state.Get().Sessions.GetShared(selected); active != nil &&
			!slices.ContainsFunc(sessions, func(session *core.SessionRecord) bool { return session.ID == active.ID }) {
			sessions = slices.Insert(sessions, 0, active)
		}
	}
	filter := *d.sessionsFilter.Get()
	if filter == nil {
		return sessions, 0
	}
	listed := len(sessions)
	kept := make([]*core.SessionRecord, 0, len(sessions))
	for _, session := range sessions {
		if (hasSelected && session.ID == selected) || d.sessionMatchesFilter(session, filter) {
			kept = append(kept, session)
		}
	}
	hidden := listed - len(kept)
	query := strings.ToLower(strings.TrimSpace(filter.Query.Value()))
	if query != "" {
		// A stable sort keeps the list's own order inside each group.
		ranks := make(map[*core.SessionRecord]uint8, len(kept))
		for _, session := range kept {
			ranks[session] = d.sessionsFilterRank(session, query)
		}
		slices.SortStableFunc(kept, func(a, b *core.SessionRecord) int {
			return int(ranks[a]) - int(ranks[b])
		})
	}
	return kept, hidden
}

func (d *DashboardState) sessionOutsideFilter(session *core.SessionRecord) bool {
	selected, ok := d.SelectedSessionID()
	if !ok || selected != session.ID {
		return false
	}
	filter := d.sessionsFilter.Get()
	return *filter != nil && !d.sessionMatchesFilter(session, *filter)
}

// sessionMatchesFilter says whether one session survives the Sessions pane
// filter: its state is admitted and the query is found in its name,
// project, profile, or target, ignoring case.
func (d *DashboardState) sessionMatchesFilter(session *core.SessionRecord, filter *SessionsFilter) bool {
	if filter.State != nil && !filter.State.admits(d.AttentionLevel(session.ID)) {
		return false
	}
	query := strings.ToLower(strings.TrimSpace(filter.Query.Value()))
	if query == "" {
		return true
	}
	if d.sessionMatchesMetadata(session, query) {
		return true
	}
	_, found := d.sessionsText.Get().matches[session.ID]
	return found
}

// isOpenableWorkspaceSession is the workspace rows Open can focus, shared
// with the cross-workspace count.
func (d *DashboardState) isOpenableWorkspaceSession(session *core.SessionRecord) bool {
	return d.isListedTopLevelSession(session, session.WorkspaceID) && d.TransitionKind(session.ID) == nil
}

// sessionsOtherWorkspaceMatches is the matching workspace sessions not
// already shown by the Sessions pane.
func (d *DashboardState) sessionsOtherWorkspaceMatches() int {
	return d.sessionOrder().otherWorkspaceMatches()
}

func (d *DashboardState) deriveOtherWorkspaceMatches() int {
	filter := *d.sessionsFilter.Get()
	if filter == nil {
		return 0
	}
	if *d.childWorkspace.Get() != nil ||
		(strings.TrimSpace(filter.Query.Value()) == "" && filter.State == nil) {
		return 0
	}
	query := strings.ToLower(strings.TrimSpace(filter.Query.Value()))
	search := d.sessionsText.Get()
	currentAnswer := strings.ToLower(search.answeredQuery) == query
	activeWorkspaceID, hasActiveWorkspace := d.ActiveWorkspaceID()
	selected, hasSelected := d.SelectedSessionID()
	count := 0
	for _, session := range d.state.Get().Sessions.All() {
		if hasActiveWorkspace && session.WorkspaceID == activeWorkspaceID {
			continue
		}
		if hasSelected && session.ID == selected {
			continue
		}
		if !d.isOpenableWorkspaceSession(session) {
			continue
		}
		if filter.State != nil && !filter.State.admits(d.AttentionLevel(session.ID)) {
			continue
		}
		if query != "" && !d.sessionMatchesMetadata(session, query) {
			if _, found := search.matches[session.ID]; !currentAnswer || !found {
				continue
			}
		}
		count++
	}
	return count
}

// sessionMatchesMetadata says whether the lower-cased query is in the
// session's name, id, project, profile, target, or branch.
func (d *DashboardState) sessionMatchesMetadata(session *core.SessionRecord, query string) bool {
	source := d.ProjectSource(session)
	branch := ""
	checkout, err := d.state.Get().Checkout(session.ID)
	if worktree, ok := checkout.(core.CheckoutManagedWorktree); err == nil && ok && worktree.Worktree != nil {
		branch = worktree.Worktree.Branch
	} else if session.LaunchBranch != nil {
		branch = *session.LaunchBranch
	}
	for _, field := range []string{
		strings.ToLower(session.ListedTitle()),
		strings.ToLower(branch),
		strings.ToLower(session.ID),
		strings.ToLower(source.Short),
		strings.ToLower(source.Full),
		strings.ToLower(session.LastProfile),
		strings.ToLower(session.TargetTemplateID),
	} {
		if strings.Contains(field, query) {
			return true
		}
	}
	return false
}

// sessionsFilterRank is where a filtered session sorts: name and branch
// matches, then user message matches, then agent message matches. A session
// held in the list only because it is selected stays first.
func (d *DashboardState) sessionsFilterRank(session *core.SessionRecord, query string) uint8 {
	if d.sessionMatchesMetadata(session, query) || d.sessionOutsideFilter(session) {
		return 0
	}
	if kind, ok := d.sessionsText.Get().matches[session.ID]; ok && kind == client.SessionTextMatchKindUser {
		return 1
	}
	return 2
}

// sessionsRankedByMatch says whether the filter's text puts the list in
// match order, which drops the project headings: a match list is not
// grouped by project.
func (d *DashboardState) sessionsRankedByMatch() bool {
	filter := *d.sessionsFilter.Get()
	return filter != nil && strings.TrimSpace(filter.Query.Value()) != ""
}

// NextSessionsTextSearch is the search the daemon should run for the
// filter's text, with its request id, or false when the newest text was
// already asked about. The caller runs it in the background and hands the
// answer to ApplySessionsText; the render loop never waits for it.
func (d *DashboardState) NextSessionsTextSearch() (uint64, string, bool) {
	query := ""
	if filter := *d.sessionsFilter.Get(); filter != nil {
		query = strings.TrimSpace(filter.Query.Value())
	}
	if query == d.sessionsText.Get().asked {
		return 0, "", false
	}
	search := d.sessionsText.Mut()
	search.requestID++
	if query == "" {
		*search = SessionsTextSearch{requestID: search.requestID}
		d.clampSelections()
		return 0, "", false
	}
	if search.sync == nil {
		search.sync = &SyncEpisode{ID: search.requestID}
	}
	// Matches for what was typed so far still hold for a longer text among
	// the rows they name; for any other text they do not.
	if !strings.HasPrefix(strings.ToLower(query), strings.ToLower(search.asked)) {
		search.matches = nil
	}
	search.asked = query
	pending := SessionsTextQueryTyped
	search.pending = &pending
	return search.requestID, query, true
}

// ApplySessionsText takes the daemon's answer to the search
// NextSessionsTextSearch handed out. An answer to an older search is
// dropped.
func (d *DashboardState) ApplySessionsText(requestID uint64, matches []client.SessionTextMatch, err error) {
	if requestID != d.sessionsText.Get().requestID {
		return
	}
	search := d.sessionsText.Mut()
	if err != nil && search.pending != nil && *search.pending == SessionsTextQuerySyncRefresh && search.sync != nil {
		failed := client.DaemonErr[client.WikiSyncStatus](err.Error())
		search.sync.Status = &failed
	}
	search.pending = nil
	if err != nil {
		search.matches = nil
		d.SetNotice("Conversation search failed: " + err.Error())
		return
	}
	search.answeredQuery = search.asked
	search.matches = make(map[string]client.SessionTextMatchKind, len(matches))
	for _, found := range matches {
		search.matches[found.SessionID] = found.Kind
	}
	d.settleSessionsFilter()
}

// beginSessionsFilter opens the Sessions filter for typing, keeping any
// state filter that is already in force.
func (d *DashboardState) beginSessionsFilter() {
	filter := d.sessionsFilter.Mut()
	if *filter == nil {
		*filter = &SessionsFilter{Query: chat.NewTextInput()}
	}
	(*filter).Editing = true
	d.FocusSessions()
}

// sessionsFilterLabel is the text the pane title shows for the filter in
// force, or empty.
func (d *DashboardState) sessionsFilterLabel() string {
	filter := *d.sessionsFilter.Get()
	if filter == nil {
		return ""
	}
	parts := []string{}
	if filter.Editing || !filter.Query.IsEmpty() {
		parts = append(parts, "/"+filter.Query.Value())
	}
	if filter.State != nil {
		parts = append(parts, filter.State.label())
	}
	if d.sessionsText.Get().isPending() {
		parts = append(parts, "searching…")
	}
	return strings.Join(parts, " · ")
}

// sessionsHiddenCount is how many rows the Sessions filter is holding back,
// or zero when no filter is in force. A shortened list that does not say it
// is shortened reads as the whole truth.
func (d *DashboardState) sessionsHiddenCount() int {
	return d.sessionOrder().hidden()
}

// escClearsSessionsFilter says whether Esc on the Sessions pane (outside
// filter text editing) clears the active filter instead of falling through
// to child-workspace exit.
//
// Top-level workspaces have no default filter, so any filter clears on Esc.
// Worker child scopes default to NotIdle: while rows are visible under that
// default filter alone, Esc returns to the parent workspace instead of
// unmasking idle children; when the filter differs from the scope default
// or hides every row (matching the empty pane's "Esc clears the filter"
// prompt), Esc clears the filter first.
func (d *DashboardState) escClearsSessionsFilter() bool {
	filter := *d.sessionsFilter.Get()
	if filter == nil {
		return false
	}
	isScopeDefault := false
	if filter.Query.IsEmpty() {
		if def := dashboardStateDefaultSessionsStateFilterFor(*d.childWorkspace.Get()); def != nil {
			isScopeDefault = filter.State != nil && *filter.State == *def
		}
	}
	return !isScopeDefault || len(d.sessionOrder().ids()) == 0
}

// handleSessionsFilterKey answers a key for the Sessions filter, or false
// when the filter does not claim it. While editing, printable keys are text
// and the arrows still move the selection; Enter keeps the filter and
// returns the letters to the pane; Esc clears the text and leaves the
// input. When not editing, the state letters narrow the list and Esc drops
// the whole filter whenever escClearsSessionsFilter holds.
func (d *DashboardState) handleSessionsFilterKey(key tea.KeyPressMsg, plain bool) bool {
	code := chat.KeyPressCode(key)
	if filter := *d.sessionsFilter.Get(); filter != nil && filter.Editing {
		d.setSessionActionFocus(nil)
		slot := d.sessionsFilter.Mut()
		filter := *slot
		switch code {
		case tea.KeyEnter:
			filter.Editing = false
			if filter.Query.IsEmpty() && filter.State == nil {
				*slot = nil
			}
		case tea.KeyEscape:
			// Esc clears the text and leaves the input. A state filter stays
			// until Esc on the pane drops it.
			filter.Query.Clear()
			filter.Editing = false
			if filter.State == nil {
				*slot = nil
			}
		default:
			// Everything else is line editing: the shared single-line editor
			// takes the readline keys, and leaves the arrows Up and Down (and
			// anything it does not know) to the pane.
			if filter.Query.HandleKey(key) == chat.EditOutcomeUnhandled {
				return false
			}
		}
		d.clampSelections()
		return true
	}
	if !plain {
		return false
	}
	switch {
	case code == tea.KeyEscape && d.escClearsSessionsFilter():
		d.clearSessionsFilter()
		return true
	case chat.IsCharKeyCode(code):
		state, ok := sessionStateFilterFromLetter(code)
		if !ok {
			return false
		}
		current := *d.sessionsFilter.Get()
		switch {
		case state == nil && current == nil:
			return false
		case state == nil:
			slot := d.sessionsFilter.Mut()
			(*slot).State = nil
			if (*slot).Query.IsEmpty() {
				*slot = nil
			}
		case current != nil:
			(*d.sessionsFilter.Mut()).State = state
		default:
			*d.sessionsFilter.Mut() = &SessionsFilter{Query: chat.NewTextInput(), State: state}
		}
	default:
		return false
	}
	d.settleSessionsFilter()
	return true
}

// clearSessionsFilter drops the whole Sessions filter, the search text and
// the state together. Esc on the pane and the × at the end of the filter's
// title label both come here.
func (d *DashboardState) clearSessionsFilter() {
	*d.sessionsFilter.Mut() = nil
	d.settleSessionsFilter()
}

// settleSessionsFilter settles the selection after the Sessions filter
// changed.
func (d *DashboardState) settleSessionsFilter() {
	d.clampSelections()
	// A filter that hides every row leaves the focus on the action row,
	// because there is nothing to select. Once a row is back, it takes the
	// focus again, so the person lands on a session rather than on Create.
	if len(d.visibleSessionIndices()) > 0 {
		d.setSessionActionFocus(nil)
	}
}

// isListedTopLevelSession lists live and transitioning top-level sessions,
// plus stopped sessions while the daemon says they need a person.
func (d *DashboardState) isListedTopLevelSession(session *core.SessionRecord, workspaceID string) bool {
	if session.WorkspaceID != workspaceID || d.state.Get().IsSubagentSession(session.ID) {
		return false
	}
	verdict := d.verdict(session.ID)
	return verdict.LiveDashboard || d.TransitionKind(session.ID) != nil ||
		(verdict.NeedsPerson && d.AttentionLevel(session.ID).NeedsPerson())
}

func (d *DashboardState) orderedSessionsUnfiltered() []*core.SessionRecord {
	sessions := d.state.Get().Sessions
	if workspace := *d.childWorkspace.Get(); workspace != nil {
		parentID := workspace.ParentID
		children := []*core.SessionRecord{}
		for _, id := range d.managedChildIDsOfKind(parentID, workspace.Kind) {
			if session := sessions.GetShared(id); session != nil {
				children = append(children, session)
			}
		}
		if workspace.Kind == core.SubagentKindWorker {
			for _, id := range slices.Sorted(maps.Keys((*d.nativeByParent.Get())[parentID])) {
				if session := sessions.GetShared(id); session != nil {
					children = append(children, session)
				}
			}
		}
		type childKey struct {
			stopped bool
			group   int
		}
		nativeAgents := *d.nativeAgents.Get()
		keys := make(map[*core.SessionRecord]childKey, len(children))
		for _, session := range children {
			group := 2
			if pane, ok := nativeAgents[session.ID]; ok {
				switch {
				case pane.Agent.State == core.NativeAgentStateRunning:
					group = 0
				case pane.Agent.Availability == core.NativeAgentAvailabilityAvailable:
					group = 1
				}
			} else if d.sessionIsWorking(session.ID) {
				group = 0
			} else if session.State == core.SessionStateRunning {
				group = 1
			}
			keys[session] = childKey{stopped: session.State == core.SessionStateStopped, group: group}
		}
		slices.SortStableFunc(children, func(a, b *core.SessionRecord) int {
			left, right := keys[a], keys[b]
			if left.stopped != right.stopped {
				if left.stopped {
					return 1
				}
				return -1
			}
			if left.group != right.group {
				return left.group - right.group
			}
			return a.CompareByCreation(b)
		})
		return children
	}
	activeWorkspaceID, ok := d.ActiveWorkspaceID()
	if !ok {
		return []*core.SessionRecord{}
	}
	active := []*core.SessionRecord{}
	for _, session := range d.listedSessionCandidates() {
		if d.isListedTopLevelSession(session, activeWorkspaceID) {
			active = append(active, session)
		}
	}
	if d.config.Get().Advanced.SessionOrder == core.SessionOrderPriority {
		facts := d.sessionFacts()
		type priorityKey struct {
			stopped  bool
			level    AttentionLevel
			activity uint64
		}
		keys := make(map[*core.SessionRecord]priorityKey, len(active))
		for _, session := range active {
			keys[session] = priorityKey{
				stopped:  session.State == core.SessionStateStopped,
				level:    facts.attentionLevel(session.ID),
				activity: d.lastActivityMs(session.ID),
			}
		}
		slices.SortStableFunc(active, func(a, b *core.SessionRecord) int {
			left, right := keys[a], keys[b]
			if left.stopped != right.stopped {
				if left.stopped {
					return 1
				}
				return -1
			}
			if left.level != right.level {
				return int(right.level) - int(left.level)
			}
			if left.activity != right.activity {
				if left.activity > right.activity {
					return -1
				}
				return 1
			}
			return a.CompareByCreation(b)
		})
		return active
	}
	// Grouping keeps this order, so stopped sessions end each project.
	slices.SortStableFunc(active, func(a, b *core.SessionRecord) int {
		leftStopped, rightStopped := a.State == core.SessionStateStopped, b.State == core.SessionStateStopped
		if leftStopped != rightStopped {
			if leftStopped {
				return 1
			}
			return -1
		}
		return a.CompareByCreation(b)
	})
	groups := map[string][]*core.SessionRecord{}
	for _, session := range active {
		key := d.ProjectSource(session).Key
		groups[key] = append(groups[key], session)
	}
	type projectGroup struct {
		short, full, key string
		sessions         []*core.SessionRecord
	}
	ordered := make([]projectGroup, 0, len(groups))
	for _, key := range slices.Sorted(maps.Keys(groups)) {
		members := groups[key]
		source := d.ProjectSource(members[0])
		ordered = append(ordered, projectGroup{
			short:    strings.ToLower(source.Short),
			full:     source.Full,
			key:      source.Key,
			sessions: members,
		})
	}
	// Display spelling must not split sessions with the same canonical key.
	slices.SortStableFunc(ordered, func(a, b projectGroup) int {
		if c := strings.Compare(a.short, b.short); c != 0 {
			return c
		}
		if c := strings.Compare(a.full, b.full); c != 0 {
			return c
		}
		return strings.Compare(a.key, b.key)
	})
	flat := make([]*core.SessionRecord, 0, len(active))
	for _, group := range ordered {
		flat = append(flat, group.sessions...)
	}
	return flat
}

func (d *DashboardState) ProjectSource(session *core.SessionRecord) core.ProjectSourceIdentity {
	identity := d.state.Get().ProjectIdentitySession(session)
	if source, ok := (*d.projectSources.Get())[identity.ID]; ok {
		return source
	}
	return identity.ProjectSource(d.config.Get())
}

func (d *DashboardState) HasResolvedProjectSource(sessionID string) bool {
	_, ok := (*d.projectSources.Get())[sessionID]
	return ok
}

func (d *DashboardState) SetProjectSource(sessionID string, source core.ProjectSourceIdentity) {
	if owns, known := d.state.Get().OwnsEnvironment(sessionID); known && owns {
		sources := d.projectSources.Mut()
		if *sources == nil {
			*sources = map[string]core.ProjectSourceIdentity{}
		}
		(*sources)[sessionID] = source
		d.clampSelections()
	}
}

func (d *DashboardState) projectKeys() []string {
	return append([]string(nil), d.sessionOrder().projectKeys()...)
}

// ProjectIsExpanded says whether this session's project draws its full
// four-row form. Projects default to expanded; only an explicit collapse
// takes that away.
func (d *DashboardState) ProjectIsExpanded(session *core.SessionRecord) bool {
	_, collapsed := (*d.collapsedProjectKeys.Get())[d.ProjectSource(session).Key]
	return !collapsed
}

// toggleProject collapses an expanded project or expands a collapsed one,
// leaving every other project alone.
func (d *DashboardState) toggleProject(projectKey string) {
	keys := d.collapsedProjectKeys.Mut()
	if _, collapsed := (*keys)[projectKey]; collapsed {
		delete(*keys, projectKey)
		return
	}
	if *keys == nil {
		*keys = map[string]struct{}{}
	}
	(*keys)[projectKey] = struct{}{}
}

func (d *DashboardState) toggleSelectedProject() {
	if session := d.selectedSession(); session != nil {
		d.toggleProject(d.ProjectSource(session).Key)
	}
}

func (d *DashboardState) toggleProjectNumber(number int) {
	if number == 0 {
		return
	}
	if d.config.Get().Advanced.SessionOrder == core.SessionOrderPriority {
		d.SetNotice("Projects are not grouped in priority order; see Settings → Advanced.")
		return
	}
	if keys := d.projectKeys(); number-1 < len(keys) {
		d.toggleProject(keys[number-1])
	}
}

// AttentionLevel is the attention level of one session, wherever it lives.
//
// A parent whose Mjolnir sub-agent is waiting on a question reads as
// waiting too. The child has no row outside the Sub-agents view, so without
// this nothing a person normally looks at would show that the child needs
// them (R11-1).
func (d *DashboardState) AttentionLevel(sessionID string) AttentionLevel {
	return d.sessionFacts().attentionLevel(sessionID)
}

// subagentsNotHandedBack is the listed titles of this parent's Mjolnir
// sub-agents that are still working or waiting on a question, oldest first.
// That is what the dashboard can see of a child that has not handed back
// its report; a child that finished its turn is taken to have handed it
// back.
func (d *DashboardState) subagentsNotHandedBack(parentID string) []string {
	ids := []string{}
	for _, id := range d.managedActiveChildIDs(parentID) {
		if owns, known := d.state.Get().OwnsEnvironment(id); !known || !owns {
			ids = append(ids, id)
		}
	}
	ids = append(ids, slices.Sorted(maps.Keys((*d.nativeByParent.Get())[parentID]))...)
	children := []*core.SessionRecord{}
	for _, id := range ids {
		if !d.verdict(id).Unfinished {
			continue
		}
		if session := d.state.Get().Sessions.GetShared(id); session != nil {
			children = append(children, session)
		}
	}
	slices.SortStableFunc(children, func(left, right *core.SessionRecord) int {
		if c := strings.Compare(left.CreatedAt, right.CreatedAt); c != 0 {
			return c
		}
		return strings.Compare(left.ID, right.ID)
	})
	titles := make([]string, 0, len(children))
	for _, child := range children {
		titles = append(titles, child.ListedTitle())
	}
	return titles
}

// InterruptFailure is one turn "Interrupt all" could not end: the session
// and why.
type InterruptFailure struct {
	SessionID string
	Error     string
}

func (d *DashboardState) InterruptAllFinished(requested int, failures []InterruptFailure) {
	if len(failures) == 0 {
		if requested == 0 {
			d.SetNotice("Nothing is running any more.")
		} else {
			d.SetNotice(fmt.Sprintf("Sent an interrupt to %s.", counted(requested, "turn", "turns")))
		}
		return
	}
	first := failures[0]
	name := first.SessionID
	if session := d.state.Get().Sessions.GetShared(first.SessionID); session != nil {
		name = session.ListedTitle()
	}
	d.SetNotice(fmt.Sprintf(
		"Could not interrupt %d of %s; %s: %s",
		len(failures), counted(requested, "turn", "turns"), name, first.Error,
	))
}

// waitingChildOf is the child whose open form makes parentID read as
// waiting when the parent itself is not, so a person is taken to that form.
func (d *DashboardState) waitingChildOf(parentID string) (string, bool) {
	holder, ok := d.formHolders.Get(parentID)
	if !ok || holder == parentID {
		return "", false
	}
	return holder, true
}

// waitingChildKind is what kind of child waitingChildOf names, for its
// label.
func (d *DashboardState) waitingChildKind(parentID string) (core.SubagentKind, bool) {
	child, ok := d.waitingChildOf(parentID)
	if !ok {
		return "", false
	}
	relation, ok := d.state.Get().Subagents.Get(child)
	if !ok {
		return "", false
	}
	return relation.Kind, true
}

// hasActiveWork says whether Stop or Restart would interrupt this session's
// own work.
func (d *DashboardState) hasActiveWork(sessionID string) bool {
	return d.verdict(sessionID).ActiveWork
}

// sessionIsWorking says whether a live, reachable session is computing
// right now: the fact the Sessions row's spinner, a parent's sub-agent count
// and the Sub-agents view's ordering all read, so they cannot disagree.
func (d *DashboardState) sessionIsWorking(sessionID string) bool {
	return d.verdict(sessionID).Working
}

// lastActivityMs is the most recent activity the dashboard knows for a
// session, for ordering sessions that share an attention level.
func (d *DashboardState) lastActivityMs(sessionID string) uint64 {
	detail, ok := (*d.sessionDetails.Get())[sessionID]
	if !ok || detail.lastActivityAtMs == nil {
		return 0
	}
	return *detail.lastActivityAtMs
}

// AttentionQueue is every unmuted session in every workspace that needs a
// person, most urgent first, then the current workspace, then newest
// activity.
func (d *DashboardState) AttentionQueue() []AttentionEntry {
	if workspaceID, ok := d.ActiveWorkspaceID(); ok {
		return d.attentionQueueForWorkspace(&workspaceID)
	}
	return d.attentionQueueForWorkspace(nil)
}

func (d *DashboardState) attentionQueueForWorkspace(workspaceID *string) []AttentionEntry {
	type queued struct {
		entry     AttentionEntry
		elsewhere bool
		activity  uint64
	}
	entries := []queued{}
	for _, session := range d.state.Get().Sessions.All() {
		if session.NotificationsIgnored {
			continue
		}
		level := d.attentionNoticeLevel(session.ID)
		if !level.NeedsPerson() {
			continue
		}
		entries = append(entries, queued{
			entry:     AttentionEntry{WorkspaceID: session.WorkspaceID, SessionID: session.ID, Level: level},
			elsewhere: workspaceID == nil || session.WorkspaceID != *workspaceID,
			activity:  d.lastActivityMs(session.ID),
		})
	}
	// Most urgent first, then the given workspace, then newest activity,
	// then by id and workspace.
	slices.SortFunc(entries, func(a, b queued) int {
		if a.entry.Level != b.entry.Level {
			return int(b.entry.Level) - int(a.entry.Level)
		}
		if a.elsewhere != b.elsewhere {
			if a.elsewhere {
				return 1
			}
			return -1
		}
		if a.activity != b.activity {
			if a.activity > b.activity {
				return -1
			}
			return 1
		}
		if c := strings.Compare(a.entry.SessionID, b.entry.SessionID); c != 0 {
			return c
		}
		return strings.Compare(a.entry.WorkspaceID, b.entry.WorkspaceID)
	})
	queue := make([]AttentionEntry, 0, len(entries))
	for _, entry := range entries {
		queue = append(queue, entry.entry)
	}
	return queue
}

// attentionSummary is the most urgent unseen level and the number of
// sessions at that level; false when no session needs a person.
func (d *DashboardState) attentionSummary(sessions []*core.SessionRecord) (AttentionLevel, int, bool) {
	var top AttentionLevel
	count := 0
	for _, session := range sessions {
		if session.NotificationsIgnored {
			continue
		}
		level := d.attentionNoticeLevel(session.ID)
		switch {
		case !level.NeedsPerson():
		case count > 0 && level == top:
			count++
		case count > 0 && level < top:
		default:
			top, count = level, 1
		}
	}
	return top, count, count > 0
}

// workspaceAttentionSummary is the badge for the sessions of one workspace,
// for its tab.
func (d *DashboardState) workspaceAttentionSummary(workspaceID string) (AttentionLevel, int, bool) {
	sessions := []*core.SessionRecord{}
	for _, session := range d.state.Get().Sessions.All() {
		if session.WorkspaceID == workspaceID {
			sessions = append(sessions, session)
		}
	}
	return d.attentionSummary(sessions)
}

// projectAttentionSummary is the badge for the visible sessions of one
// project, for a folded heading.
func (d *DashboardState) projectAttentionSummary(projectKey string) (AttentionLevel, int, bool) {
	order := d.sessionOrder()
	sessions := []*core.SessionRecord{}
	for index, id := range order.ids() {
		if project, ok := order.project(index); !ok || project != projectKey {
			continue
		}
		if session := d.state.Get().Sessions.GetShared(id); session != nil {
			sessions = append(sessions, session)
		}
	}
	return d.attentionSummary(sessions)
}

// sessionsAttentionSummary is the badge for every session the Sessions pane
// is showing, for its title while the pane is minimized.
func (d *DashboardState) sessionsAttentionSummary() (AttentionLevel, int, bool) {
	return d.attentionSummary(d.orderedSessions())
}

// attentionBadgeSummary is the badge for every session that needs a
// person, anywhere.
func (d *DashboardState) attentionBadgeSummary() (AttentionLevel, int, bool) {
	queue := d.AttentionQueue()
	if len(queue) == 0 {
		return 0, 0, false
	}
	top := queue[0].Level
	for _, entry := range queue {
		top = max(top, entry.Level)
	}
	count := 0
	for _, entry := range queue {
		if entry.Level == top {
			count++
		}
	}
	return top, count, true
}

// stepAttention moves to the next (1) or previous (-1) session in the
// attention queue, counting from the selected session when it is in the
// queue and from the top otherwise. Consecutive steps retain their starting
// workspace preference so switching tabs cannot strand notifications.
func (d *DashboardState) stepAttention(delta int) DashboardAction {
	selected, hasSelected := d.SelectedSessionID()
	navigation := d.attentionNavigation
	if navigation != nil && (!hasSelected || selected != navigation.sessionID) {
		navigation = nil
	}
	activeWorkspaceID := func() *string {
		if id, ok := d.ActiveWorkspaceID(); ok {
			return &id
		}
		return nil
	}
	var workspaceID *string
	if navigation != nil {
		workspaceID = navigation.workspaceID
	} else {
		workspaceID = activeWorkspaceID()
	}
	queue := d.attentionQueueForWorkspace(workspaceID)
	if len(queue) == 0 {
		d.attentionNavigation = nil
		d.SetNotice("Nothing is waiting for you.")
		return DashboardActionNone{}
	}
	var from *string
	if navigation != nil {
		from = &navigation.queueSessionID
	} else if hasSelected {
		from = &selected
	}
	position := -1
	if from != nil {
		position = slices.IndexFunc(queue, func(entry AttentionEntry) bool { return entry.SessionID == *from })
	}
	// Resolving the selected notification ends this traversal.
	if active := activeWorkspaceID(); position < 0 && !dashboardSessionsSameWorkspace(workspaceID, active) {
		workspaceID = active
		queue = d.AttentionQueue()
	}
	var target AttentionEntry
	switch {
	case position >= 0:
		length := len(queue)
		target = queue[((position+delta)%length+length)%length]
	case delta < 0:
		target = queue[len(queue)-1]
	default:
		target = queue[0]
	}
	// A parent waiting only on a child leads to the child's form, in the
	// Sub-agents or Reviewers workspace where that child is listed.
	var child *string
	if target.Level == AttentionLevelWaiting {
		if d.notificationCards != nil {
			if card := d.notificationCards.GetShared(target.SessionID); card != nil &&
				card.Holder != nil && *card.Holder != target.SessionID {
				holder := *card.Holder
				child = &holder
			}
		} else if holder, ok := d.waitingChildOf(target.SessionID); ok {
			child = &holder
		}
	}
	queueSessionID := target.SessionID
	var sessionID string
	var action DashboardAction
	session := d.state.Get().Sessions.GetShared(target.SessionID)
	switch {
	case child != nil:
		action = d.focusSessionFromCPUReport(*child)
		sessionID = *child
	case !d.state.Get().IsSubagentSession(target.SessionID) && session != nil && !session.State.IsActive():
		// The row's conversation area says what to do with it; the notice
		// names the session and what failed.
		notice := d.NoticeNamingWorkspace(target.SessionID, fmt.Sprintf(
			"%s stopped: %s",
			d.sessionNoticeName(target.SessionID),
			d.sessionFailureText(target.SessionID),
		))
		d.SetNotice(notice)
		action = d.moveToSession(target.WorkspaceID, target.SessionID, false)
		sessionID = target.SessionID
	default:
		action = d.focusSessionFromCPUReport(target.SessionID)
		sessionID = target.SessionID
	}
	d.attentionNavigation = &AttentionNavigation{
		sessionID:      sessionID,
		workspaceID:    workspaceID,
		queueSessionID: queueSessionID,
	}
	return action
}

// dashboardSessionsSameWorkspace compares two optional workspace ids as Rust
// compares `Option<&str>`.
func dashboardSessionsSameWorkspace(a, b *string) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return *a == *b
}

// focusSessionAnywhere moves the dashboard to one session, wherever it
// lives.
//
// A session in another workspace is reached by recording it as that
// workspace's selection and asking the host to switch: the host restores
// the selection when the tab changes and opens its conversation, exactly as
// it does for a tab the person clicks.
func (d *DashboardState) focusSessionAnywhere(workspaceID string, sessionID string) DashboardAction {
	return d.moveToSession(workspaceID, sessionID, true)
}

func (d *DashboardState) moveToSession(workspaceID string, sessionID string, open bool) DashboardAction {
	if *d.childWorkspace.Get() != nil {
		d.switchChildScope(nil)
		d.sessionsScroll = 0
	}
	if active := *d.activeWorkspaceID.Get(); active == nil || *active != workspaceID {
		if d.workspaceViews == nil {
			d.workspaceViews = map[string]WorkspaceViewState{}
		}
		view, ok := d.workspaceViews[workspaceID]
		if !ok {
			view = WorkspaceViewState{
				paneSizes:            core.NewPaneSizes(),
				conversationLayout:   core.NewConversationLayout(),
				collapsedProjectKeys: map[string]struct{}{},
				focus:                FocusPrompt,
			}
		}
		if open {
			view.focus = FocusPrompt
		} else {
			view.focus = FocusSessions
		}
		d.workspaceViews[workspaceID] = view
		d.pendingNavigation = &PendingNavigation{sessionID: sessionID, open: open}
		return DashboardActionSelectWorkspace{WorkspaceID: workspaceID}
	}
	if session := d.state.Get().Sessions.GetShared(sessionID); session != nil {
		key := d.ProjectSource(session).Key
		delete(*d.collapsedProjectKeys.Mut(), key)
	}
	d.SelectActiveSession(sessionID)
	d.clearFilterHidingSelection()
	if open {
		return d.openSelectedSession()
	}
	d.focus = FocusSessions
	return DashboardActionNone{}
}

// focusSessionFromCPUReport opens a session selected from the CPU report.
// Managed sub-agents live in their direct parent's virtual workspace, so
// crossing workspaces carries both the child selection and the parent view
// to restore.
func (d *DashboardState) focusSessionFromCPUReport(sessionID string) DashboardAction {
	session := d.state.Get().Sessions.GetShared(sessionID)
	if session == nil {
		return DashboardActionNone{}
	}
	sessionWorkspace := session.WorkspaceID
	relation, isChild := d.state.Get().Subagents.Get(sessionID)
	d.CancelModal()

	if isChild {
		parentID := relation.ParentSessionID
		parent := d.state.Get().Sessions.GetShared(parentID)
		if parent == nil {
			return DashboardActionNone{}
		}
		parentWorkspace := parent.WorkspaceID
		if active, ok := d.ActiveWorkspaceID(); ok && active == parentWorkspace {
			if d.state.Get().Subagents.Index(sessionID).Kind == core.SubagentKindReviewer {
				d.OpenReviewerWorkspace(parentID)
			} else {
				d.OpenSubagentWorkspace(parentID)
			}
			d.SelectActiveSession(sessionID)
			d.clearFilterHidingSelection()
			return d.openSelectedSession()
		}

		action := d.focusSessionAnywhere(parentWorkspace, sessionID)
		if _, switching := action.(DashboardActionSelectWorkspace); switching {
			d.navigationSubagentParent = &parentID
		}
		return action
	}

	return d.focusSessionAnywhere(sessionWorkspace, sessionID)
}

// clearFilterHidingSelection drops a Sessions filter that hides the session
// the person just went to. The list keeps a selected row even when the
// filter does not match it, so this asks the filter, not the list.
func (d *DashboardState) clearFilterHidingSelection() {
	selected, ok := d.SelectedSessionID()
	if !ok {
		return
	}
	if session := d.state.Get().Sessions.GetShared(selected); session != nil && d.sessionOutsideFilter(session) {
		*d.sessionsFilter.Mut() = nil
	}
}

// SetGitStatus records a fresh reading of a session's checkout, or why
// there is none.
func (d *DashboardState) SetGitStatus(sessionID string, result client.DaemonResult[core.SessionGitStatus]) {
	if !d.state.Get().Sessions.ContainsKey(sessionID) {
		return
	}
	if d.gitStatus == nil {
		d.gitStatus = map[string]client.DaemonResult[core.SessionGitStatus]{}
	}
	d.gitStatus[sessionID] = result
}

// requestGitProbe asks the host to read one session's checkout now, and
// remembers the request so the periodic probe does not repeat it straight
// away.
func (d *DashboardState) requestGitProbe(sessionID string) DashboardAction {
	if d.gitProbeAt == nil {
		d.gitProbeAt = map[string]time.Time{}
	}
	d.gitProbeAt[sessionID] = time.Now()
	return DashboardActionProbeGitStatus{SessionID: sessionID}
}

// GitProbeCandidates is the visible live sessions whose checkout has not
// been read in the last minute, oldest reading first, at most limit of
// them: the host reads every one returned, and each is recorded as read.
func (d *DashboardState) GitProbeCandidates(now time.Time, limit int) []string {
	const refresh = 60 * time.Second
	type probe struct {
		probed *time.Time
		id     string
	}
	due := []probe{}
	operations := *d.sessionOperations.Get()
	for _, session := range d.orderedSessions() {
		if !session.State.IsActive() || session.Target == nil {
			continue
		}
		if _, busy := operations[session.ID]; busy {
			continue
		}
		if d.TransitionKind(session.ID) != nil {
			continue
		}
		var probed *time.Time
		if at, ok := d.gitProbeAt[session.ID]; ok {
			probed = &at
		}
		if probed != nil && now.Sub(*probed) < refresh {
			continue
		}
		due = append(due, probe{probed: probed, id: session.ID})
	}
	// Never-read sessions first, then the oldest reading, then by id.
	slices.SortFunc(due, func(a, b probe) int {
		switch {
		case a.probed == nil && b.probed != nil:
			return -1
		case a.probed != nil && b.probed == nil:
			return 1
		case a.probed != nil && !a.probed.Equal(*b.probed):
			return a.probed.Compare(*b.probed)
		}
		return strings.Compare(a.id, b.id)
	})
	// Only what the host will read this pass counts as read. The rest stay
	// due, so the next pass reaches them.
	due = due[:min(len(due), max(limit, 0))]
	if d.gitProbeAt == nil {
		d.gitProbeAt = map[string]time.Time{}
	}
	ids := make([]string, 0, len(due))
	for _, entry := range due {
		d.gitProbeAt[entry.id] = now
		ids = append(ids, entry.id)
	}
	return ids
}

// gitRowText is the branch text a session row carries, when its checkout
// has been read and is a repository.
func (d *DashboardState) gitRowText(sessionID string) (string, bool) {
	result, ok := d.gitStatus[sessionID]
	if !ok || result.Err != nil {
		return "", false
	}
	text := result.Ok.RowTextWith(chat.ASCII())
	return text, text != ""
}

// markAllRead clears the unread marker on the sessions the active workspace
// lists, which is exactly the set its tab badge counts. A session with no
// row contributes to no badge, so leaving it unread hides nothing.
func (d *DashboardState) markAllRead() DashboardAction {
	listed := []string{}
	for _, session := range d.orderedSessionsUnfiltered() {
		listed = append(listed, session.ID)
	}
	receipts := []client.DaemonPair[string, uint64]{}
	for _, sessionID := range listed {
		// Rust's Option<u64> max: any value beats None.
		var through *uint64
		raise := func(value uint64) {
			if through == nil || value > *through {
				through = &value
			}
		}
		if frontier, ok := d.unreadFrontiers.Get(sessionID); ok {
			raise(frontier)
		}
		if target, ok := d.notificationDismissals.Get().Get(sessionID); ok {
			if readThrough, ok := target.(core.NotificationDismissalReadThrough); ok {
				raise(readThrough.Value)
			}
		}
		if d.notificationCards != nil {
			if card := d.notificationCards.GetShared(sessionID); card != nil {
				for _, entry := range card.Entries {
					if entry.Kind == core.NotificationKindImportant {
						raise(entry.CompletedOrdinal)
					}
				}
			}
		}
		if through == nil {
			continue
		}
		state := d.state.Mut()
		session := state.Sessions.GetMut(sessionID)
		if session == nil {
			continue
		}
		if *through > session.ViewedThroughEventOrdinal {
			session.ViewedThroughEventOrdinal = *through
			if detail, ok := (*d.sessionDetails.Mut())[sessionID]; ok {
				detail.clearUnread()
			}
			receipts = append(receipts, client.DaemonPair[string, uint64]{First: sessionID, Second: *through})
		}
	}
	if len(receipts) == 0 {
		d.SetNotice("No unread sessions in this workspace.")
		return DashboardActionNone{}
	}
	d.SetNotice("Marked this workspace read; questions and failures stay flagged.")
	return DashboardActionMarkAllRead{Receipts: receipts}
}

func (d *DashboardState) compatibleProfiles(sessionID string) []CompatibleProfile {
	if d.SessionRecord(sessionID) == nil {
		return []CompatibleProfile{}
	}
	profiles := []CompatibleProfile{}
	config := d.config.Get()
	for _, id := range slices.Sorted(maps.Keys(config.Profiles)) {
		if d.profileOffered(id) {
			profiles = append(profiles, CompatibleProfile{ID: id, Kind: config.Profiles[id].Kind})
		}
	}
	return profiles
}

// profileChoice is one row of the profile picker tables: a blank indent,
// the profile id, the harness, and the two quota percentages remaining. The
// picker pads the cells into aligned columns.
func (d *DashboardState) profileChoice(id string, harness core.HarnessKind) PickerChoice {
	weekly, fiveHour := d.profileQuotaCells(id)
	return pickerChoiceTable([]PickerCell{
		pickerCellBlank(),
		pickerCellText(id),
		pickerCellText(harness.DisplayName()),
		weekly,
		fiveHour,
	})
}

// newWizardProfileChoice is one expanded profile row in the new-session
// picker: blank PROVIDER column, individual profile name in the PROFILE
// column, and quota cells.
func (d *DashboardState) newWizardProfileChoice(id string) PickerChoice {
	weekly, fiveHour := d.profileQuotaCells(id)
	return pickerChoiceTable([]PickerCell{
		pickerCellBlank(),
		pickerCellBlank(),
		pickerCellText(id),
		weekly,
		fiveHour,
	})
}

// predictedProfileForCandidates is a best-effort preview of which offered
// profile in candidates would be selected by quota load balancing, using
// the same preference and remaining-headroom signals already cached on the
// dashboard.
func (d *DashboardState) predictedProfileForCandidates(candidates []string) (string, bool) {
	var best *profilePrediction
	for _, id := range candidates {
		key, ok := d.profilePredictionKey(id)
		if !ok {
			continue
		}
		// min_by_key keeps the first of equal keys.
		if best == nil || key.compare(*best) < 0 {
			found := key
			best = &found
		}
	}
	if best == nil {
		return "", false
	}
	return best.id, true
}

// compare is the lexicographic order of Rust's prediction tuple.
func (p profilePrediction) compare(other profilePrediction) int {
	if c := cmp.Compare(p.tier, other.tier); c != 0 {
		return c
	}
	if c := cmp.Compare(p.preferRank, other.preferRank); c != 0 {
		return c
	}
	if c := cmp.Compare(p.primary, other.primary); c != 0 {
		return c
	}
	if c := cmp.Compare(p.fiveHour, other.fiveHour); c != 0 {
		return c
	}
	return strings.Compare(p.id, other.id)
}

// predictedProviderProfile is a preview of which profile in provider would
// be used when launching with that provider row selected.
func (d *DashboardState) predictedProviderProfile(provider core.QuotaProvider) (string, bool) {
	candidates := []string{}
	for id, profile := range d.offeredProfiles() {
		if profileProvider(id, &profile, &d.quotaProviders) == provider {
			candidates = append(candidates, id)
		}
	}
	return d.predictedProfileForCandidates(candidates)
}

// predictedAutoPrimaryProfile is a preview of which profile Auto (Primary)
// would select among offered profiles whose discovered capabilities offer a
// configured primary model.
func (d *DashboardState) predictedAutoPrimaryProfile() (string, bool) {
	config := d.config.Get()
	choices := config.Subagents.PrimaryChoices()
	if len(choices) == 0 {
		return "", false
	}
	identities := make([]string, len(choices))
	for index, choice := range choices {
		identities[index] = d.profileCapabilities.ModelID(config, choice.Model)
	}
	candidates := []string{}
	for id, profile := range d.offeredProfiles() {
		entry, ok := d.profileCapabilities.Profiles[profile.CapabilitiesKey(id)]
		if !ok || entry.Choices.Kind != core.CapabilityStateReady {
			continue
		}
		models := entry.Choices.Value.Models
		matchesPrimary := false
		for index, choice := range choices {
			offer, ok := core.Offer(models, identities[index], choice.Model)
			if !ok {
				continue
			}
			if choice.Effort == nil {
				matchesPrimary = true
				break
			}
			efforts, ok := entry.Efforts[offer.Value]
			if ok && efforts.Kind == core.CapabilityStateReady && slices.ContainsFunc(efforts.Value, func(effort core.SessionConfigChoice) bool {
				return effort.Value == *choice.Effort
			}) {
				matchesPrimary = true
				break
			}
		}
		if matchesPrimary {
			candidates = append(candidates, id)
		}
	}
	return d.predictedProfileForCandidates(candidates)
}

// windowRemainingPercent is quotaRemainingPercent of a window the report
// may not have (Rust's `window().and_then(quota_remaining_percent)`).
func windowRemainingPercent(window *client.QuotaWindow) (uint8, bool) {
	if window == nil {
		return 0, false
	}
	return quotaRemainingPercent(window)
}

func (d *DashboardState) profilePredictionKey(id string) (profilePrediction, bool) {
	preferRank := slices.Index(d.config.Get().Quota.Prefer, id)
	if preferRank < 0 {
		preferRank = math.MaxInt
	}
	unknown := func(tier uint8) (profilePrediction, bool) {
		return profilePrediction{tier: tier, preferRank: preferRank, primary: math.MaxUint8, fiveHour: math.MaxUint8, id: id}, true
	}
	if _, refreshing := d.quotaRefreshing[id]; refreshing {
		return unknown(1)
	}
	quota, ok := d.quotas[id]
	if !ok {
		return unknown(1)
	}
	if quota.Error != nil {
		return unknown(4)
	}
	if quota.IsUsagePriced() {
		return unknown(3)
	}
	primary, hasPrimary := windowRemainingPercent(quota.SummaryWindow())
	fiveHour, hasFiveHour := windowRemainingPercent(quota.FiveHourWindow())
	if weeklyQuotaExhausted(&quota) || (hasPrimary && primary == 0) || (hasFiveHour && fiveHour == 0) {
		return profilePrediction{}, false
	}
	if !hasPrimary {
		return unknown(1)
	}
	fiveHourUsed := uint8(0)
	if hasFiveHour {
		fiveHourUsed = 100 - min(fiveHour, 100)
	}
	tier := uint8(0)
	// Subscriptions at the 1% reserve floor rank behind unknown quota,
	// matching profile_scheduler's reserve-floor tiering.
	if primary <= 1 || (hasFiveHour && fiveHour <= 1) {
		tier = 2
	}
	return profilePrediction{
		tier:       tier,
		preferRank: preferRank,
		primary:    100 - min(primary, 100),
		fiveHour:   fiveHourUsed,
		id:         id,
	}, true
}

// profileQuotaCells is the WEEKLY and 5H cells of a profile picker row, as
// percentages remaining coloured by how much headroom they leave. The
// five-hour cell stays blank whenever there is no five-hour figure worth
// reading: a profile that is refreshing, failing, usage-priced, or out of
// weekly quota altogether.
func (d *DashboardState) profileQuotaCells(id string) (PickerCell, PickerCell) {
	plain := func(text string) (PickerCell, PickerCell) {
		return pickerCellText(text), pickerCellBlank()
	}
	if _, refreshing := d.quotaRefreshing[id]; refreshing {
		return plain("refreshing")
	}
	quota, ok := d.quotas[id]
	if !ok {
		return plain("refreshing")
	}
	if quota.Error != nil {
		label, ok := quota.ErrorLabel()
		if !ok {
			label = "unavailable"
		}
		return plain(label)
	}
	if label, ok := quota.WindowlessLabel(); ok {
		return plain(label)
	}
	if quota.Harness == core.HarnessKindAntigravity {
		var pools []core.AntigravityQuotaPool
		if profile, ok := d.config.Get().EnabledProfile(id); ok {
			pools = profile.AntigravityPools()
		}
		poolCell := func(weekly bool) PickerCell {
			parts := []string{}
			for _, pool := range pools {
				// A pool without this allowance has nothing to show: Argon
				// reports no five-hour limit, and only plans that keep Argon
				// apart report its pool at all.
				report := quota.ForAntigravityPool(pool)
				window := report.FiveHourWindow()
				if weekly {
					window = report.WeeklyWindow()
				}
				if window == nil {
					continue
				}
				value := "?"
				if remaining, ok := quotaRemainingPercent(window); ok {
					value = fmt.Sprintf("%d%%", remaining)
				}
				parts = append(parts, fmt.Sprintf("%s %s", pool.Label(), value))
			}
			return pickerCellText(strings.Join(parts, " / "))
		}
		return poolCell(true), poolCell(false)
	}
	primary, ok := windowRemainingPercent(quota.SummaryWindow())
	if !ok {
		return plain("unavailable")
	}
	percent := func(value uint8) PickerCell {
		return pickerCellStyled(fmt.Sprintf("%d%%", value), chat.Style{}.Foreground(headroomColor(value)))
	}
	fiveHour := pickerCellBlank()
	if quota.WeeklyWindow() != nil && !weeklyQuotaExhausted(&quota) {
		if remaining, ok := windowRemainingPercent(quota.FiveHourWindow()); ok {
			fiveHour = percent(remaining)
		}
	}
	return percent(primary), fiveHour
}

// selectedContainerSession is the selected session, if its target template
// creates a container.
func (d *DashboardState) selectedContainerSession() *core.SessionRecord {
	session := d.commandSession()
	if session == nil || !d.verdict(session.ID).ContainerSettings.Offered() {
		return nil
	}
	return session
}

func (d *DashboardState) configIsEmpty() bool {
	config := d.config.Get()
	for range config.EnabledProfiles() {
		return len(config.Targets) == 0
	}
	return true
}

// SessionPreflightGeneration is the identity for supervised launch checks;
// cancellation invalidates late replies.
func (d *DashboardState) SessionPreflightGeneration() uint64 {
	return d.sessionPreflightGeneration
}

// invalidateSessionPreflight invalidates an in-flight session preflight
// while keeping its modal open. Selection changes use this so a late result
// cannot describe a different target or repository bundle.
func (d *DashboardState) invalidateSessionPreflight() {
	d.sessionPreflightGeneration++
}

func (d *DashboardState) ApplySuspendPreflight(preflight core.SuspendPreflight) DashboardAction {
	shouldQuickSuspend := false
	if confirm, ok := d.mode.(ModeConfirm); ok {
		if suspend, ok := confirm.confirmation.(ConfirmationSuspendSession); ok && suspend.SessionID == preflight.SessionID {
			quick := !preflight.HasBlockers() && !preflight.HasWarnings() &&
				!suspend.Interrupting && len(suspend.ChildrenNotHandedBack) == 0
			received := preflight
			suspend.Preflight = &received
			suspend.Checking = false
			confirm.confirmation = suspend
			if !quick {
				confirm.refreshForm()
			}
			shouldQuickSuspend = quick
		}
	}
	if shouldQuickSuspend {
		sessionID := preflight.SessionID
		d.CancelModal()
		return DashboardActionSuspend{SessionID: sessionID, Consent: core.SuspendConsent{}}
	}
	return DashboardActionNone{}
}

// DismissSuspendPreflight closes the suspend confirmation dialog if it is
// still waiting on sessionID's preflight, leaving any unrelated modal
// untouched.
func (d *DashboardState) DismissSuspendPreflight(sessionID string) bool {
	matches := false
	if confirm, ok := d.mode.(ModeConfirm); ok {
		if suspend, ok := confirm.confirmation.(ConfirmationSuspendSession); ok {
			matches = suspend.Checking && suspend.SessionID == sessionID
		}
	}
	if matches {
		d.CancelModal()
	}
	return matches
}

func (d *DashboardState) CancelModal() {
	d.sessionPreflightGeneration++
	if _, ok := d.mode.(ModeWorkspaceManager); ok {
		d.workspaceManagementGeneration++
	}
	d.mode = ModeDashboard{}
	d.RebuildResumeRows()
}

func (d *DashboardState) quotaPools() []ProviderQuota {
	return providerPools(d.config.Get(), d.quotas, &d.quotaProviders, epochSeconds())
}

func (d *DashboardState) quotaRows() []QuotaRow {
	return selectableRows(d.quotaPools(), d.quotaExpanded)
}

func (d *DashboardState) selectedQuotaRow() QuotaRow {
	pools := d.quotaPools()
	rows := selectableRows(pools, d.quotaExpanded)
	// A profile's row can change type between refreshes (a status row
	// becomes a pool or lane after its first reading, and back on a failure),
	// so follow the profile to its row or the pool it joined before falling
	// back to its previous pool.
	profileRow := func(id string) QuotaRow {
		for _, row := range rows {
			switch row := row.(type) {
			case QuotaRowProfile:
				if row.ID == id {
					return row
				}
			case QuotaRowLane:
				if row.ID == id {
					return row
				}
			}
		}
		for _, row := range rows {
			pool, ok := row.(QuotaRowPool)
			if !ok {
				continue
			}
			for _, provider := range pools {
				index := slices.IndexFunc(provider.Pools, func(candidate QuotaPool) bool { return candidate.Key == pool.Key })
				if index < 0 {
					continue
				}
				if slices.ContainsFunc(provider.Pools[index].Members, func(member PoolMember) bool { return member.ID == id }) {
					return row
				}
				break
			}
		}
		return nil
	}
	if selected := d.quotaSelection; selected != nil {
		if slices.Contains(rows, selected) {
			return selected
		}
		var followed QuotaRow
		switch selected := selected.(type) {
		case QuotaRowProfile:
			followed = profileRow(selected.ID)
		case QuotaRowLane:
			followed = profileRow(selected.ID)
		}
		if followed != nil {
			return followed
		}
		if key, ok := selected.pool(); ok && slices.Contains(rows, QuotaRow(QuotaRowPool{Key: key})) {
			return QuotaRowPool{Key: key}
		}
	}
	if len(rows) > 0 {
		return rows[0]
	}
	return nil
}

func (d *DashboardState) toggleQuotaPool() {
	row := d.selectedQuotaRow()
	if row == nil {
		return
	}
	key, ok := row.pool()
	if !ok {
		return
	}
	_, lane := row.(QuotaRowLane)
	if index := slices.Index(d.quotaExpanded, key); lane || index >= 0 {
		if index >= 0 {
			d.quotaExpanded = slices.Delete(d.quotaExpanded, index, index+1)
		}
	} else {
		at, _ := slices.BinarySearchFunc(d.quotaExpanded, key, PoolKey.compare)
		d.quotaExpanded = slices.Insert(d.quotaExpanded, at, key)
	}
	d.quotaSelection = QuotaRowPool{Key: key}
	quota := FocusQuota
	d.recenterOnSelection = &quota
}

func (d *DashboardState) focusLenFor(focus Focus) int {
	switch focus {
	case FocusSessions:
		return len(d.visibleSessionIndices())
	case FocusTargets:
		return len(d.capacityDetails)
	case FocusQuota:
		return len(d.quotaRows())
	default:
		return 0
	}
}

// setSelectionFor moves the focused list's selection to index, counted
// among the rows currently on screen.
func (d *DashboardState) setSelectionFor(focus Focus, index int) bool {
	d.recenterOnSelection = nil
	if focus == FocusSessions {
		d.setSessionActionFocus(nil)
		// Choosing a row, even the current one, shows it again.
		d.sessionsWheelScrolledFrom = nil
	}
	switch focus {
	case FocusSessions:
		sessions := d.orderedSessions()
		visible := d.visibleSessionIndices()
		if index < 0 || index >= len(visible) || visible[index] >= len(sessions) {
			return false
		}
		next := sessions[visible[index]].ID
		if selected, ok := d.SelectedSessionID(); ok && selected == next {
			return false
		}
		d.SelectActiveSession(next)
		return true
	case FocusTargets:
		if d.capacityIndex != index {
			d.capacityIndex = index
			return true
		}
		return false
	case FocusQuota:
		var next QuotaRow
		if rows := d.quotaRows(); index >= 0 && index < len(rows) {
			next = rows[index]
		}
		changed := d.quotaSelection != next
		d.quotaSelection = next
		quota := FocusQuota
		d.recenterOnSelection = &quota
		return changed
	default:
		return false
	}
}

func (d *DashboardState) selectionFor(focus Focus) int {
	switch focus {
	case FocusSessions:
		index, _ := d.selectedVisibleIndex()
		return index
	case FocusTargets:
		return d.capacityIndex
	case FocusQuota:
		selected := d.selectedQuotaRow()
		return max(slices.IndexFunc(d.quotaRows(), func(row QuotaRow) bool {
			return selected != nil && row == selected
		}), 0)
	default:
		return 0
	}
}

func (d *DashboardState) moveSelection(delta int) {
	if d.focus == FocusQuota && d.scrollTallQuotaRow(delta) {
		return
	}
	d.scrollSelectionFor(d.focus, delta)
}

// scrollTallQuotaRow: a selected quota row taller than the viewport (a
// wrapped note) shows its remaining lines before a single-step move leaves
// it.
func (d *DashboardState) scrollTallQuotaRow(delta int) bool {
	viewport := d.quotaViewport
	rendered := d.quotaRenderedSelection
	if rendered == nil {
		return false
	}
	first, last := rendered.first, rendered.last
	offset := d.quotaScroll
	var next int
	switch {
	case delta > 0 && last+1 > offset+viewport && last+1-first > viewport:
		next = offset + 1
	case delta < 0 && offset > first && last+1-first > viewport:
		next = offset - 1
	default:
		return false
	}
	d.quotaScroll = next
	if d.recenterOnSelection != nil && *d.recenterOnSelection == FocusQuota {
		d.recenterOnSelection = nil
	}
	return true
}

// scrollSessionsView scrolls the Sessions list by delta rows without moving
// the selection. The renderer clamps the offset to the list and leaves it
// alone until the selection changes.
func (d *DashboardState) scrollSessionsView(delta int) {
	d.recenterOnSelection = nil
	if selected, ok := d.SelectedSessionID(); ok {
		d.sessionsWheelScrolledFrom = &selected
	} else {
		d.sessionsWheelScrolledFrom = nil
	}
	d.sessionsScroll = max(d.sessionsScroll+delta, 0)
}

func (d *DashboardState) scrollSelectionFor(focus Focus, delta int) {
	length := d.focusLenFor(focus)
	if length == 0 {
		d.setSelectionFor(focus, 0)
		return
	}
	if _, visible := d.selectedVisibleIndex(); focus == FocusSessions && !visible {
		if delta < 0 {
			d.setSelectionFor(focus, length-1)
		} else {
			d.setSelectionFor(focus, 0)
		}
		return
	}
	index := min(d.selectionFor(focus), max(length-1, 0))
	previous := index
	moveIndex(&index, length, delta)
	d.setSelectionFor(focus, index)
	if index != previous {
		d.recenterOnSelection = &focus
	}
}

// clampSelections clamps auxiliary list positions. Session selection
// belongs to navigation and is never reassigned by row filtering or
// refresh.
func (d *DashboardState) clampSelections() {
	projectKeys := d.projectKeys()
	// Rust's retain takes the set mutably whether or not it removes a key.
	keys := d.collapsedProjectKeys.Mut()
	for key := range *keys {
		if !slices.Contains(projectKeys, key) {
			delete(*keys, key)
		}
	}
	quotaSelection := d.selectedQuotaRow()
	if d.quotaSelection != quotaSelection && d.focus == FocusQuota {
		quota := FocusQuota
		d.recenterOnSelection = &quota
	}
	d.quotaSelection = quotaSelection
	d.capacityIndex = min(d.capacityIndex, max(len(d.capacityDetails)-1, 0))
}

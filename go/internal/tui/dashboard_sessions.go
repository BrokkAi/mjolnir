// Port of mj-tui/src/dashboard_sessions.rs.

package tui

import (
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
	panic("unported: mj-tui/src/dashboard_sessions.rs presentation_attention")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::derive_sessions_rows")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::derive_ordered_sessions")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::derive_other_workspace_matches")
}

// sessionMatchesMetadata says whether the lower-cased query is in the
// session's name, id, project, profile, target, or branch.
func (d *DashboardState) sessionMatchesMetadata(session *core.SessionRecord, query string) bool {
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::session_matches_metadata")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::next_sessions_text_search")
}

// ApplySessionsText takes the daemon's answer to the search
// NextSessionsTextSearch handed out. An answer to an older search is
// dropped.
func (d *DashboardState) ApplySessionsText(requestID uint64, matches []client.SessionTextMatch, err error) {
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::apply_sessions_text")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::handle_sessions_filter_key")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::ordered_sessions_unfiltered")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::subagents_not_handed_back")
}

// InterruptFailure is one turn "Interrupt all" could not end: the session
// and why.
type InterruptFailure struct {
	SessionID string
	Error     string
}

func (d *DashboardState) InterruptAllFinished(requested int, failures []InterruptFailure) {
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::interrupt_all_finished")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::attention_queue_for_workspace")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::step_attention")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::move_to_session")
}

// focusSessionFromCPUReport opens a session selected from the CPU report.
// Managed sub-agents live in their direct parent's virtual workspace, so
// crossing workspaces carries both the child selection and the parent view
// to restore.
func (d *DashboardState) focusSessionFromCPUReport(sessionID string) DashboardAction {
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::focus_session_from_cpu_report")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::git_probe_candidates")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::mark_all_read")
}

func (d *DashboardState) compatibleProfiles(sessionID string) []CompatibleProfile {
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::compatible_profiles")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::predicted_profile_for_candidates")
}

// predictedProviderProfile is a preview of which profile in provider would
// be used when launching with that provider row selected.
func (d *DashboardState) predictedProviderProfile(provider core.QuotaProvider) (string, bool) {
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::predicted_provider_profile")
}

// predictedAutoPrimaryProfile is a preview of which profile Auto (Primary)
// would select among offered profiles whose discovered capabilities offer a
// configured primary model.
func (d *DashboardState) predictedAutoPrimaryProfile() (string, bool) {
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::predicted_auto_primary_profile")
}

func (d *DashboardState) profilePredictionKey(id string) (profilePrediction, bool) {
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::profile_prediction_key")
}

// profileQuotaCells is the WEEKLY and 5H cells of a profile picker row, as
// percentages remaining coloured by how much headroom they leave. The
// five-hour cell stays blank whenever there is no five-hour figure worth
// reading: a profile that is refreshing, failing, usage-priced, or out of
// weekly quota altogether.
func (d *DashboardState) profileQuotaCells(id string) (PickerCell, PickerCell) {
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::profile_quota_cells")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::apply_suspend_preflight")
}

// DismissSuspendPreflight closes the suspend confirmation dialog if it is
// still waiting on sessionID's preflight, leaving any unrelated modal
// untouched.
func (d *DashboardState) DismissSuspendPreflight(sessionID string) bool {
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::dismiss_suspend_preflight")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::selected_quota_row")
}

func (d *DashboardState) toggleQuotaPool() {
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::toggle_quota_pool")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::set_selection_for")
}

func (d *DashboardState) selectionFor(focus Focus) int {
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::selection_for")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::scroll_tall_quota_row")
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
	panic("unported: mj-tui/src/dashboard_sessions.rs DashboardState::clamp_selections")
}

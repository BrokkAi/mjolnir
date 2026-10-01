use super::*;

use mj_chat::text_input::EditOutcome;
use ratatui::style::Style;

use mj_client::daemon::SessionTextMatchKind;
use mj_client::quota::API_LABEL;

use crate::render::{headroom_color, quota_remaining_percent, weekly_quota_exhausted};

/// How much a session needs a person right now, from least to most.
///
/// The order is the order a person wants to be interrupted in: a failure
/// first, then a session nothing can be learned about because its worker is
/// unreachable, then a question the agent cannot proceed without, then a
/// finished answer nobody has read, then work in progress, then idle, then
/// anything stopped or still starting. Rows and priority sorting retain the
/// session's status; the attention queue and badges omit failures already read
/// in this terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AttentionLevel {
    Inactive,
    Idle,
    Working,
    Unread,
    Waiting,
    Unreachable,
    Failed,
}

impl AttentionLevel {
    /// Whether the attention queue lists a session at this level.
    pub fn needs_person(self) -> bool {
        self >= Self::Unread
    }
}

/// The turns Interrupt all ends: Mjolnir sessions by id, and harness-native
/// sub-agents by owner session and child id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InterruptAllTargets {
    pub sessions: Vec<String>,
    pub native: Vec<(String, String)>,
}

impl InterruptAllTargets {
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty() && self.native.is_empty()
    }

    pub fn len(&self) -> usize {
        self.sessions.len() + self.native.len()
    }
}

/// One session the attention queue would take a person to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttentionEntry {
    pub workspace_id: String,
    pub session_id: String,
    pub level: AttentionLevel,
}

/// The attention level from the same facts the row symbol reads.
///
/// `in_operation` is a launch, resume, move, or stop the daemon is running
/// for the session; the session is busy on the person's behalf, not waiting.
/// `transition_failed` is a launch, resume, move, or stop that stopped with
/// an error, which the row already draws as a failure.
pub(crate) fn attention_level(
    detail: Option<&SessionDetail>,
    review: Option<&RuntimeReviewView>,
    state: SessionState,
    unreachable: bool,
    in_operation: bool,
    transition_failed: bool,
) -> AttentionLevel {
    use mj_core::review::driver::TurnReviewPhase;
    use mj_core::review::verdict::ReviewVerdict;

    if transition_failed {
        return AttentionLevel::Failed;
    }
    if in_operation {
        return AttentionLevel::Working;
    }
    match state {
        SessionState::Lost | SessionState::Error | SessionState::DestroyedWithDataLoss => {
            return AttentionLevel::Failed;
        }
        SessionState::Disconnected => return AttentionLevel::Unreachable,
        SessionState::StartupCleanup => return AttentionLevel::Failed,
        // A parked sub-agent is idle by definition: its turn ended, its parent
        // was told, and it has no worker to report anything else.
        SessionState::Parked => return AttentionLevel::Idle,
        SessionState::Stopped
        | SessionState::Provisioning
        | SessionState::Checkpointing
        | SessionState::Closing
        | SessionState::Destroying => return AttentionLevel::Inactive,
        SessionState::Running => {}
    }
    if unreachable {
        return AttentionLevel::Unreachable;
    }
    // A review that failed outranks a pending question, so the row, the queue,
    // and the badge all report the failure first.
    let review_level = review
        .filter(|review| review.activity_label().is_some())
        .map(|review| match &review.phase {
            TurnReviewPhase::Verdict(ReviewVerdict::Failed { .. })
            | TurnReviewPhase::Forwarding { error: Some(_), .. } => AttentionLevel::Failed,
            _ if review.is_working() => AttentionLevel::Working,
            TurnReviewPhase::Verdict(ReviewVerdict::Clean) => AttentionLevel::Unread,
            _ => AttentionLevel::Waiting,
        });
    if review_level == Some(AttentionLevel::Failed) {
        return AttentionLevel::Failed;
    }
    if detail.is_some_and(|detail| !detail.pending_elicitations.is_empty()) {
        return AttentionLevel::Waiting;
    }
    if let Some(level) = review_level {
        return level;
    }
    let Some(detail) = detail else {
        return AttentionLevel::Idle;
    };
    if matches!(
        detail.activity.state().last_known(),
        mj_core::activity::ActivityState::CheckingContinuation
    ) {
        return AttentionLevel::Working;
    }
    if detail.awaiting_input && detail.current_turn_started_at.is_none() {
        AttentionLevel::Waiting
    } else if matches!(
        detail.activity.state().last_known(),
        mj_core::activity::ActivityState::Expecting { .. }
    ) || !detail.activity.is_idle(detail.current_turn_started_at)
    {
        AttentionLevel::Working
    } else if detail.has_unread() {
        AttentionLevel::Unread
    } else {
        AttentionLevel::Idle
    }
}

/// The attention level of `session` from its own facts alone. Only
/// [`crate::session_view::SessionFacts`] calls this; everything else reads
/// the level it keeps.
pub(crate) fn own_attention_level(
    dashboard: &DashboardState,
    session_id: &str,
    session: &SessionRecord,
) -> AttentionLevel {
    attention_level(
        dashboard.session_details.get(session_id),
        dashboard.session_review(session_id),
        session.state,
        dashboard.unreachable_sessions.contains(session_id),
        dashboard.session_operations.contains_key(session_id)
            || dashboard.transition_kind(session_id).is_some(),
        dashboard.transition_failure_kind(session_id).is_some(),
    )
}

/// The daemon's answer to the Sessions filter's text: which sessions have it
/// in a user or agent message. The text itself stays in [`SessionsFilter`];
/// this holds only what a background search found for it.
#[derive(Debug, Default)]
pub(crate) struct SessionsTextSearch {
    /// Bumped for every search asked for, and for a filter that no longer has
    /// text, so an answer to an older search is recognised and dropped.
    request_id: u64,
    /// The text last asked about. `matches` answers it, or an earlier prefix
    /// of it while the newer search is out.
    asked: String,
    /// A search is out and has not answered.
    pending: bool,
    matches: BTreeMap<String, SessionTextMatchKind>,
}

impl SessionsTextSearch {
    /// Whether a conversation search is out and has not answered.
    pub(crate) fn is_pending(&self) -> bool {
        self.pending
    }
}

impl DashboardState {
    pub(crate) fn command_session_id(&self) -> Option<&str> {
        self.command_session_override
            .as_deref()
            .or(self.selected_session_id())
    }

    pub(crate) fn command_session(&self) -> Option<&SessionRecord> {
        self.state.sessions.get(self.command_session_id()?)
    }

    pub(crate) fn selected_session(&self) -> Option<&SessionRecord> {
        self.state.sessions.get(
            self.command_session_override
                .as_deref()
                .or(self.selected_session_id())?,
        )
    }

    /// The live sessions the Sessions pane is showing, as indices into
    /// [`Self::ordered_sessions`].
    pub(crate) fn visible_session_indices(&self) -> Vec<usize> {
        self.session_order()
            .rows()
            .iter()
            .filter_map(|row| match row {
                SessionsRow::Session { index, .. } => Some(*index),
                _ => None,
            })
            .collect()
    }

    /// The rows the Sessions pane draws at every explicit size: a heading per
    /// project and one row per live session.
    ///
    /// Standard and Maximized use four-line session rows; Minimized keeps only
    /// each session's top summary line. Focus never changes the representation.
    pub(crate) fn sessions_rows(&self) -> Vec<SessionsRow> {
        self.session_order().rows().to_vec()
    }

    /// The rows for `sessions`, in order, for [`Self::session_order`] to
    /// keep. `numbered` says whether more than one project is listed.
    pub(crate) fn derive_sessions_rows(
        &self,
        sessions: &[&SessionRecord],
        numbered: bool,
    ) -> Vec<SessionsRow> {
        // Two projects can share a short name, in which case both need their
        // full names to stay distinguishable.
        let mut short_names = BTreeMap::<String, BTreeSet<String>>::new();
        for session in sessions {
            let source = self.project_source(session);
            short_names
                .entry(source.short)
                .or_default()
                .insert(source.key);
        }
        let grouped = self.config.advanced.session_order == SessionOrder::Project
            && !self.sessions_ranked_by_match();
        let mut rows = Vec::new();
        let mut previous = None;
        let mut number = 0;
        for (index, session) in sessions.iter().enumerate() {
            let source = self.project_source(session);
            if grouped && previous.as_ref() != Some(&source.key) {
                number += 1;
                let label = if short_names
                    .get(&source.short)
                    .is_some_and(|projects| projects.len() > 1)
                {
                    source.full.clone()
                } else {
                    source.short.clone()
                };
                rows.push(SessionsRow::ProjectHeading {
                    key: source.key.clone(),
                    label,
                    number: (numbered && number <= 9).then_some(number),
                });
                previous = Some(source.key.clone());
            }
            rows.push(SessionsRow::Session {
                index,
                expanded: !self.collapsed_project_keys.contains(&source.key),
            });
        }
        rows
    }

    /// Where the selection sits among the rows on screen, for the table's
    /// highlight. `None` when nothing is selected or the selection is not on
    /// screen.
    pub(crate) fn selected_visible_index(&self) -> Option<usize> {
        let selected = self.selected_session_id()?;
        let order = self.session_order();
        let index = order.position(selected)?;
        order
            .rows()
            .iter()
            .filter_map(|row| match row {
                SessionsRow::Session { index, .. } => Some(*index),
                _ => None,
            })
            .position(|visible| visible == index)
    }

    /// Sessions visible in the selected workspace, grouped by project and
    /// ordered by creation. Stopped sessions appear only among a parent's
    /// sub-agents, after the rest; in-flight transitions remain visible
    /// regardless. The controller may feed all workspaces into one
    /// state snapshot; the tab is the local view filter.
    pub(crate) fn ordered_sessions(&self) -> Vec<&SessionRecord> {
        self.session_order()
            .ids()
            .iter()
            .filter_map(|id| self.state.sessions.get(id))
            .collect()
    }

    /// Where a session is in [`Self::ordered_sessions`], if it is listed.
    pub(crate) fn ordered_session_position(&self, session_id: &str) -> Option<usize> {
        self.session_order().position(session_id)
    }

    /// [`Self::ordered_sessions`] worked out from the inputs, and how many
    /// sessions the filter holds back, for [`Self::session_order`] to keep.
    pub(crate) fn derive_ordered_sessions(&self) -> (Vec<&SessionRecord>, usize) {
        let mut sessions = self.ordered_sessions_unfiltered();
        let selected = self.selected_session_id();
        if let Some(active) = selected.and_then(|id| self.state.sessions.get(id))
            && !sessions.iter().any(|session| session.id == active.id)
        {
            sessions.insert(0, active);
        }
        let Some(filter) = self.sessions_filter.as_ref() else {
            return (sessions, 0);
        };
        let listed = sessions.len();
        let mut kept = sessions
            .into_iter()
            .filter(|session| {
                Some(session.id.as_str()) == selected
                    || self.session_matches_filter(session, filter)
            })
            .collect::<Vec<_>>();
        let hidden = listed - kept.len();
        let query = filter.query.value().trim().to_lowercase();
        if !query.is_empty() {
            // A stable sort keeps the list's own order inside each group.
            kept.sort_by_key(|session| self.sessions_filter_rank(session, &query));
        }
        (kept, hidden)
    }

    pub(crate) fn session_outside_filter(&self, session: &SessionRecord) -> bool {
        Some(session.id.as_str()) == self.selected_session_id()
            && self
                .sessions_filter
                .as_ref()
                .is_some_and(|filter| !self.session_matches_filter(session, filter))
    }

    /// Whether one session survives the Sessions pane filter: its state is
    /// admitted and the query is found in its name, project, profile, or
    /// target, ignoring case.
    fn session_matches_filter(&self, session: &SessionRecord, filter: &SessionsFilter) -> bool {
        if let Some(state) = filter.state
            && !state.admits(self.attention_level(&session.id))
        {
            return false;
        }
        let query = filter.query.value().trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        self.session_matches_metadata(session, &query)
            || self.sessions_text.matches.contains_key(&session.id)
    }

    /// Whether the lower-cased `query` is in the session's name, id, project,
    /// profile, target, or branch.
    fn session_matches_metadata(&self, session: &SessionRecord, query: &str) -> bool {
        let source = self.project_source(session);
        let branch = session
            .managed_worktree
            .as_ref()
            .map(|worktree| worktree.branch.as_str())
            .or(session.launch_branch.as_deref())
            .unwrap_or_default()
            .to_lowercase();
        [
            session.display_title().to_lowercase(),
            session.listed_title().to_lowercase(),
            branch,
            session.id.to_lowercase(),
            source.short.to_lowercase(),
            source.full.to_lowercase(),
            session.last_profile.to_lowercase(),
            session.target_template_id.to_lowercase(),
        ]
        .iter()
        .any(|field| field.contains(query))
    }

    /// Where a filtered session sorts: name and branch matches, then user
    /// message matches, then agent message matches. A session held in the list
    /// only because it is selected stays first.
    fn sessions_filter_rank(&self, session: &SessionRecord, query: &str) -> u8 {
        if self.session_matches_metadata(session, query) || self.session_outside_filter(session) {
            return 0;
        }
        match self.sessions_text.matches.get(&session.id) {
            Some(SessionTextMatchKind::User) => 1,
            _ => 2,
        }
    }

    /// Whether the filter's text puts the list in match order, which drops the
    /// project headings: a match list is not grouped by project.
    pub(crate) fn sessions_ranked_by_match(&self) -> bool {
        self.sessions_filter
            .as_ref()
            .is_some_and(|filter| !filter.query.value().trim().is_empty())
    }

    /// The search the daemon should run for the filter's text, with its
    /// request id, or `None` when the newest text was already asked about. The
    /// caller runs it in the background and hands the answer to
    /// [`Self::apply_sessions_text`]; the render loop never waits for it.
    pub fn next_sessions_text_search(&mut self) -> Option<(u64, String)> {
        let query = self
            .sessions_filter
            .as_ref()
            .map(|filter| filter.query.value().trim().to_owned())
            .unwrap_or_default();
        let search = &mut self.sessions_text;
        if query == search.asked {
            return None;
        }
        search.request_id = search.request_id.wrapping_add(1);
        if query.is_empty() {
            **search = SessionsTextSearch {
                request_id: search.request_id,
                ..SessionsTextSearch::default()
            };
            self.clamp_selections();
            return None;
        }
        // Matches for what was typed so far still hold for a longer text
        // among the rows they name; for any other text they do not.
        if !query
            .to_lowercase()
            .starts_with(&search.asked.to_lowercase())
        {
            search.matches.clear();
        }
        search.asked.clone_from(&query);
        search.pending = true;
        Some((search.request_id, query))
    }

    /// Takes the daemon's answer to the search
    /// [`Self::next_sessions_text_search`] handed out. An answer to an older
    /// search is dropped.
    pub fn apply_sessions_text(
        &mut self,
        request_id: u64,
        result: Result<Vec<mj_client::daemon::SessionTextMatch>, String>,
    ) {
        if request_id != self.sessions_text.request_id {
            return;
        }
        self.sessions_text.pending = false;
        match result {
            Ok(matches) => {
                self.sessions_text.matches = matches
                    .into_iter()
                    .map(|found| (found.session_id, found.kind))
                    .collect();
                self.settle_sessions_filter();
            }
            Err(error) => {
                self.sessions_text.matches.clear();
                self.set_notice(format!("Conversation search failed: {error}"));
            }
        }
    }

    /// Opens the Sessions filter for typing, keeping any state filter that
    /// is already in force.
    pub(crate) fn begin_sessions_filter(&mut self) {
        let filter = self
            .sessions_filter
            .get_or_insert_with(SessionsFilter::default);
        filter.editing = true;
        self.focus_sessions();
    }

    /// The text the pane title shows for the filter in force, or empty.
    pub(crate) fn sessions_filter_label(&self) -> String {
        let Some(filter) = self.sessions_filter.as_ref() else {
            return String::new();
        };
        let mut parts = Vec::new();
        if filter.editing || !filter.query.is_empty() {
            parts.push(format!("/{}", filter.query.value()));
        }
        if let Some(state) = filter.state {
            parts.push(state.label().to_owned());
        }
        if self.sessions_text.pending {
            parts.push("searching…".to_owned());
        }
        parts.join(" · ")
    }

    /// How many rows the Sessions filter is holding back, or zero when no
    /// filter is in force. A shortened list that does not say it is shortened
    /// reads as the whole truth.
    pub(crate) fn sessions_hidden_count(&self) -> usize {
        self.session_order().hidden()
    }

    /// Answers a key for the Sessions filter, or `None` when the filter does
    /// not claim it. While editing, printable keys are text and the arrows
    /// still move the selection; `Enter` keeps the filter and returns the
    /// letters to the pane; `Esc` clears the text and leaves the input. When not editing, the state letters narrow the
    /// list and `Esc` drops the whole filter.
    pub(crate) fn handle_sessions_filter_key(&mut self, key: KeyEvent, plain: bool) -> Option<()> {
        let editing = self
            .sessions_filter
            .as_ref()
            .is_some_and(|filter| filter.editing);
        if editing {
            self.set_session_action_focus(None);
            let filter = self.sessions_filter.as_mut()?;
            match key.code {
                KeyCode::Enter => {
                    filter.editing = false;
                    if filter.query.is_empty() && filter.state.is_none() {
                        *self.sessions_filter = None;
                    }
                }
                KeyCode::Esc => {
                    // Esc clears the text and leaves the input. A state
                    // filter stays until Esc on the pane drops it.
                    filter.query.clear();
                    filter.editing = false;
                    if filter.state.is_none() {
                        *self.sessions_filter = None;
                    }
                }
                // Everything else is line editing: the shared single-line
                // editor takes the readline keys, and leaves the arrows Up and
                // Down (and anything it does not know) to the pane.
                _ => {
                    if matches!(filter.query.handle_key(key), EditOutcome::Unhandled) {
                        return None;
                    }
                }
            }
            self.clamp_selections();
            return Some(());
        }
        if !plain {
            return None;
        }
        match key.code {
            KeyCode::Esc if self.sessions_filter.is_some() => {
                self.clear_sessions_filter();
                return Some(());
            }
            KeyCode::Char(letter) if SessionStateFilter::from_letter(letter).is_some() => {
                let state = SessionStateFilter::from_letter(letter)?;
                match (state, self.sessions_filter.as_mut()) {
                    (None, None) => return None,
                    (None, Some(filter)) => {
                        filter.state = None;
                        if filter.query.is_empty() {
                            *self.sessions_filter = None;
                        }
                    }
                    (Some(state), Some(filter)) => filter.state = Some(state),
                    (Some(state), None) => {
                        *self.sessions_filter = Some(SessionsFilter {
                            query: mj_chat::text_input::TextInput::new(),
                            state: Some(state),
                            editing: false,
                        });
                    }
                }
            }
            _ => return None,
        }
        self.settle_sessions_filter();
        Some(())
    }

    /// Drops the whole Sessions filter, the search text and the state
    /// together. `Esc` on the pane and the `×` at the end of the filter's
    /// title label both come here.
    pub(crate) fn clear_sessions_filter(&mut self) {
        *self.sessions_filter = None;
        self.settle_sessions_filter();
    }

    /// Settles the selection after the Sessions filter changed.
    fn settle_sessions_filter(&mut self) {
        self.clamp_selections();
        // A filter that hides every row leaves the focus on the action row,
        // because there is nothing to select. Once a row is back, it takes the
        // focus again, so the person lands on a session rather than on Create.
        if !self.visible_session_indices().is_empty() {
            self.set_session_action_focus(None);
        }
    }

    /// Whether the Sessions pane lists `session` as a top-level row of
    /// `workspace_id`: live or mid-transition. Stopped sessions are listed
    /// only by the resume dialog. Terminal failures such as a lost or data-loss session have
    /// no row, so the badges and the attention queue must not count them
    /// either; they are reachable only through the resume dialog.
    pub(crate) fn is_listed_top_level_session(
        &self,
        session: &SessionRecord,
        workspace_id: &str,
    ) -> bool {
        session.workspace_id == workspace_id
            && !self.state.is_subagent_session(&session.id)
            && (session.state.is_active() || self.transition_kind(&session.id).is_some())
    }

    fn ordered_sessions_unfiltered(&self) -> Vec<&SessionRecord> {
        if let Some(parent_id) = self.subagent_parent_id.as_deref() {
            let mut children = self
                .managed_child_ids(parent_id)
                .into_iter()
                .filter_map(|id| self.state.sessions.get(&id))
                .collect::<Vec<_>>();
            children.extend(
                self.native_by_parent
                    .get(parent_id)
                    .into_iter()
                    .flatten()
                    .filter_map(|id| self.state.sessions.get(id)),
            );
            children.sort_by_cached_key(|session| {
                let group = if let Some(pane) = self.native_agents.get(&session.id) {
                    if pane.agent.state == mj_core::native_agent::NativeAgentState::Running {
                        0
                    } else if pane.agent.availability
                        == mj_core::native_agent::NativeAgentAvailability::Available
                    {
                        1
                    } else {
                        2
                    }
                } else if self.session_is_working(&session.id) {
                    0
                } else if session.state == SessionState::Running {
                    1
                } else {
                    2
                };
                (
                    session.state == SessionState::Stopped,
                    group,
                    session.creation_order_key(),
                )
            });
            return children;
        }
        let Some(active_workspace_id) = self.active_workspace_id.as_deref() else {
            return Vec::new();
        };
        let active = self
            .listed_session_candidates()
            .into_iter()
            .filter(|session| self.is_listed_top_level_session(session, active_workspace_id))
            .collect::<Vec<_>>();
        let mut active = active;
        if self.config.advanced.session_order == SessionOrder::Priority {
            let facts = self.session_facts();
            active.sort_by_cached_key(|session| {
                (
                    session.state == SessionState::Stopped,
                    std::cmp::Reverse(facts.attention_level(&session.id)),
                    std::cmp::Reverse(self.last_activity_ms(&session.id)),
                    session.creation_order_key(),
                )
            });
            return active;
        }
        // Grouping keeps this order, so stopped sessions end each project.
        active.sort_by_cached_key(|session| {
            (
                session.state == SessionState::Stopped,
                session.creation_order_key(),
            )
        });
        let mut groups = BTreeMap::<String, Vec<&SessionRecord>>::new();
        for session in active {
            groups
                .entry(self.project_source(session).key)
                .or_default()
                .push(session);
        }
        let mut groups = groups.into_values().collect::<Vec<_>>();
        // Display spelling must not split sessions with the same canonical key.
        groups.sort_by_cached_key(|sessions| {
            let source = self.project_source(sessions[0]);
            (source.short.to_lowercase(), source.full, source.key)
        });
        groups.into_iter().flatten().collect()
    }

    pub fn project_source(&self, session: &SessionRecord) -> ProjectSourceIdentity {
        let session = self.state.project_identity_session(session);
        self.project_sources
            .get(&session.id)
            .cloned()
            .unwrap_or_else(|| session.project_source(&self.config))
    }

    pub fn has_resolved_project_source(&self, session_id: &str) -> bool {
        self.project_sources.contains_key(session_id)
    }

    pub fn set_project_source(&mut self, session_id: &str, source: ProjectSourceIdentity) {
        if self.state.sessions.contains_key(session_id) {
            self.project_sources.insert(session_id.to_owned(), source);
            self.clamp_selections();
        }
    }

    pub(crate) fn project_keys(&self) -> Vec<String> {
        self.session_order().project_keys().to_vec()
    }

    /// Whether this session's project draws its full four-row form. Projects
    /// default to expanded; only an explicit collapse takes that away.
    pub fn project_is_expanded(&self, session: &SessionRecord) -> bool {
        !self
            .collapsed_project_keys
            .contains(&self.project_source(session).key)
    }

    /// Collapses an expanded project or expands a collapsed one, leaving
    /// every other project alone.
    pub(crate) fn toggle_project(&mut self, project_key: &str) {
        if !self.collapsed_project_keys.remove(project_key) {
            self.collapsed_project_keys.insert(project_key.to_owned());
        }
    }

    pub(crate) fn toggle_selected_project(&mut self) {
        let key = self
            .selected_session()
            .map(|session| self.project_source(session).key);
        if let Some(key) = key {
            self.toggle_project(&key);
        }
    }

    pub(crate) fn toggle_project_number(&mut self, number: usize) {
        if number == 0 {
            return;
        }
        if self.config.advanced.session_order == SessionOrder::Priority {
            self.set_notice("Projects are not grouped in priority order; see Settings → Advanced.");
            return;
        }
        if let Some(key) = self.project_keys().get(number - 1).cloned() {
            self.toggle_project(&key);
        }
    }

    /// The attention level of one session, wherever it lives.
    ///
    /// A parent whose Mjolnir sub-agent is waiting on a question reads as
    /// waiting too. The child has no row outside the Sub-agents view, so
    /// without this nothing a person normally looks at would show that the
    /// child needs them (R11-1).
    pub fn attention_level(&self, session_id: &str) -> AttentionLevel {
        self.session_facts().attention_level(session_id)
    }

    /// The listed titles of this parent's Mjolnir sub-agents that are still
    /// working or waiting on a question, oldest first. That is what the
    /// dashboard can see of a child that has not handed back its report; a
    /// child that finished its turn is taken to have handed it back.
    pub(crate) fn subagents_not_handed_back(&self, parent_id: &str) -> Vec<String> {
        let mut children = self
            .managed_active_child_ids(parent_id)
            .into_iter()
            .filter(|id| self.has_unfinished_turn(id))
            .filter_map(|id| self.state.sessions.get(&id))
            .collect::<Vec<_>>();
        children.sort_by(|left, right| {
            (left.created_at.as_str(), left.id.as_str())
                .cmp(&(right.created_at.as_str(), right.id.as_str()))
        });
        children
            .into_iter()
            .map(|child| child.listed_title().to_owned())
            .collect()
    }

    /// Whether the session's own turn is still going: working, or holding a
    /// question the agent asked in that turn.
    fn has_unfinished_turn(&self, session_id: &str) -> bool {
        matches!(
            self.own_attention_level(session_id),
            AttentionLevel::Working | AttentionLevel::Waiting
        )
    }

    /// What Interrupt all stops for `root`: every session in its Mjolnir
    /// sub-agent tree whose turn is unfinished, and every running
    /// harness-native sub-agent under any of them that can be stopped.
    pub(crate) fn interrupt_all_targets(&self, root: &str) -> InterruptAllTargets {
        let mut targets = InterruptAllTargets::default();
        let mut visited = BTreeSet::new();
        let mut pending = vec![root.to_owned()];
        while let Some(id) = pending.pop() {
            if !visited.insert(id.clone()) {
                continue;
            }
            if let Some(pane) = self.native_agents.get(&id) {
                if pane.agent.capabilities.cancel && !pane.stopping {
                    targets.native.push((
                        pane.agent.owner_session_id.clone(),
                        pane.agent.session_id.clone(),
                    ));
                }
            } else if self.has_unfinished_turn(&id) {
                targets.sessions.push(id.clone());
            }
            pending.extend(self.managed_active_child_ids(&id));
            pending.extend(
                self.native_running_by_parent
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
        }
        targets
    }

    /// Reports how Interrupt all went, and lets a native sub-agent whose stop
    /// failed be stopped again.
    pub fn interrupt_all_finished(
        &mut self,
        targets: &InterruptAllTargets,
        failures: Vec<(String, String)>,
    ) {
        for (owner, child) in &targets.native {
            let view_id = mj_core::native_agent::view_id(owner, child);
            if failures.iter().any(|(id, _)| *id == view_id)
                && let Some(pane) = self.native_agents.get_mut(&view_id)
            {
                pane.stopping = false;
            }
        }
        let Some((id, error)) = failures.first() else {
            self.set_notice(format!(
                "Sent an interrupt to {}.",
                crate::widgets::counted(targets.len(), "turn", "turns")
            ));
            return;
        };
        let name = self
            .state
            .sessions
            .get(id)
            .map_or(id.as_str(), |session| session.listed_title());
        self.set_notice(format!(
            "Could not interrupt {} of {}; {name}: {error}",
            failures.len(),
            crate::widgets::counted(targets.len(), "turn", "turns"),
        ));
    }

    /// The first of this parent's Mjolnir sub-agents that is waiting on a
    /// question, with that question.
    pub(crate) fn subagent_question(
        &self,
        parent_id: &str,
    ) -> Option<(&SessionRecord, &mj_core::elicitation::ElicitationRequest)> {
        let child = self.session_facts().waiting_child(parent_id)?.to_owned();
        let child = self.state.sessions.get(&child)?;
        let question = self
            .session_details
            .get(&child.id)?
            .pending_elicitations
            .first()?;
        Some((child, question))
    }

    /// The attention level from the session's own facts alone.
    fn own_attention_level(&self, session_id: &str) -> AttentionLevel {
        self.session_facts().own_attention_level(session_id)
    }

    /// Whether a live, reachable session is computing right now: the fact the
    /// Sessions row's spinner, a parent's sub-agent count and the Sub-agents
    /// view's ordering all read, so they cannot disagree.
    pub(crate) fn session_is_working(&self, session_id: &str) -> bool {
        self.state
            .sessions
            .get(session_id)
            .is_some_and(|session| session.state == SessionState::Running)
            && !self.unreachable_sessions.contains(session_id)
            && self.session_details.get(session_id).is_some_and(|detail| {
                detail.activity.is_working(
                    detail.current_turn_started_at,
                    !detail.pending_elicitations.is_empty(),
                )
            })
    }

    /// The most recent activity the dashboard knows for a session, for
    /// ordering sessions that share an attention level.
    fn last_activity_ms(&self, session_id: &str) -> u64 {
        self.session_details
            .get(session_id)
            .and_then(|detail| detail.last_activity_at_ms)
            .unwrap_or(0)
    }

    /// Every top-level session in every workspace that needs a person, most
    /// urgent first and newest activity first within a level.
    pub fn attention_queue(&self) -> Vec<AttentionEntry> {
        let mut entries = self
            .listed_session_candidates()
            .into_iter()
            .filter(|session| self.is_listed_top_level_session(session, &session.workspace_id))
            .filter_map(|session| {
                let level = self.attention_notice_level(&session.id);
                level.needs_person().then(|| {
                    (
                        std::cmp::Reverse(level),
                        std::cmp::Reverse(self.last_activity_ms(&session.id)),
                        session.id.clone(),
                        session.workspace_id.clone(),
                    )
                })
            })
            .collect::<Vec<_>>();
        entries.sort();
        entries
            .into_iter()
            .map(|(level, _, session_id, workspace_id)| AttentionEntry {
                workspace_id,
                session_id,
                level: level.0,
            })
            .collect()
    }

    /// The most urgent unseen level and the number of sessions at that level.
    fn attention_summary<'a>(
        &self,
        sessions: impl IntoIterator<Item = &'a SessionRecord>,
    ) -> Option<(AttentionLevel, usize)> {
        sessions
            .into_iter()
            .filter_map(|session| {
                let level = self.attention_notice_level(&session.id);
                level.needs_person().then_some(level)
            })
            .fold(None, |summary, level| match summary {
                Some((top, count)) if level == top => Some((top, count + 1)),
                Some((top, count)) if level < top => Some((top, count)),
                Some(_) => Some((level, 1)),
                None => Some((level, 1)),
            })
    }

    /// The badge for the sessions of one workspace, for its tab.
    pub(crate) fn workspace_attention_summary(
        &self,
        workspace_id: &str,
    ) -> Option<(AttentionLevel, usize)> {
        self.attention_summary(
            self.listed_session_candidates()
                .into_iter()
                .filter(|session| self.is_listed_top_level_session(session, workspace_id)),
        )
    }

    /// The badge for the visible sessions of one project, for a folded
    /// heading.
    pub(crate) fn project_attention_summary(
        &self,
        project_key: &str,
    ) -> Option<(AttentionLevel, usize)> {
        let order = self.session_order();
        self.attention_summary(
            order
                .ids()
                .iter()
                .enumerate()
                .filter(|(index, _)| order.project(*index) == Some(project_key))
                .filter_map(|(_, id)| self.state.sessions.get(id)),
        )
    }

    /// The badge for every session the Sessions pane is showing, for its
    /// title while the pane is minimized.
    pub(crate) fn sessions_attention_summary(&self) -> Option<(AttentionLevel, usize)> {
        self.attention_summary(self.ordered_sessions())
    }

    /// The badge for every session that needs a person, anywhere.
    pub(crate) fn attention_badge_summary(&self) -> Option<(AttentionLevel, usize)> {
        let queue = self.attention_queue();
        let top = queue.iter().map(|entry| entry.level).max()?;
        Some((top, queue.iter().filter(|entry| entry.level == top).count()))
    }

    /// Moves to the next (`1`) or previous (`-1`) session in the attention
    /// queue, counting from the selected session when it is in the queue and
    /// from the top otherwise.
    pub(crate) fn step_attention(&mut self, delta: isize) -> DashboardAction {
        let queue = self.attention_queue();
        if queue.is_empty() {
            self.set_notice("Nothing is waiting for you.");
            return DashboardAction::None;
        }
        let position = self
            .selected_session_id()
            .and_then(|selected| queue.iter().position(|entry| entry.session_id == selected));
        let target = match position {
            Some(position) => {
                let len = queue.len() as isize;
                queue[(position as isize + delta).rem_euclid(len) as usize].clone()
            }
            None if delta < 0 => queue[queue.len() - 1].clone(),
            None => queue[0].clone(),
        };
        self.focus_session_anywhere(&target.workspace_id, &target.session_id)
    }

    /// Moves the dashboard to one session, wherever it lives.
    ///
    /// A session in another workspace is reached by recording it as that
    /// workspace's selection and asking the host to switch: the host restores
    /// the selection when the tab changes and opens its conversation, exactly
    /// as it does for a tab the person clicks.
    pub(crate) fn focus_session_anywhere(
        &mut self,
        workspace_id: &str,
        session_id: &str,
    ) -> DashboardAction {
        if self.subagent_parent_id.is_some() {
            self.close_subagent_workspace();
        }
        if self.active_workspace_id.as_deref() != Some(workspace_id) {
            let view = self
                .workspace_views
                .entry(workspace_id.to_owned())
                .or_insert_with(|| WorkspaceViewState {
                    sessions_scroll: 0,
                    targets_scroll: 0,
                    quota_scroll: 0,
                    capacity_index: 0,
                    quota_index: 0,
                    pane_sizes: PaneSizes::default(),
                    conversation_layout: mj_core::workspace::ConversationLayout::default(),
                    collapsed_project_keys: BTreeSet::new(),
                    focus: Focus::Prompt,
                });
            view.focus = Focus::Prompt;
            self.navigation_session = Some(session_id.to_owned());
            return DashboardAction::SelectWorkspace {
                workspace_id: workspace_id.to_owned(),
            };
        }
        if let Some(session) = self.state.sessions.get(session_id) {
            let key = self.project_source(session).key;
            self.collapsed_project_keys.remove(&key);
        }
        // A filter that hides the session the person asked for is no longer
        // what they want.
        if !self
            .ordered_sessions()
            .iter()
            .any(|session| session.id == session_id)
        {
            *self.sessions_filter = None;
        }
        self.select_active_session(session_id);
        self.open_selected_session()
    }

    /// Records a fresh reading of a session's checkout, or why there is none.
    pub fn set_git_status(
        &mut self,
        session_id: String,
        result: Result<mj_core::local_git::SessionGitStatus, String>,
    ) {
        if self.state.sessions.contains_key(&session_id) {
            self.git_status.insert(session_id, result);
        }
    }

    /// Asks the host to read one session's checkout now, and remembers the
    /// request so the periodic probe does not repeat it straight away.
    pub(crate) fn request_git_probe(&mut self, session_id: &str) -> DashboardAction {
        self.git_probe_at
            .insert(session_id.to_owned(), Instant::now());
        DashboardAction::ProbeGitStatus {
            session_id: session_id.to_owned(),
        }
    }

    /// The visible live sessions whose checkout has not been read in the last
    /// minute, oldest reading first, at most `limit` of them: the host reads
    /// every one returned, and each is recorded as read.
    pub fn git_probe_candidates(&mut self, now: Instant, limit: usize) -> Vec<String> {
        const REFRESH: Duration = Duration::from_secs(60);
        let mut due = self
            .ordered_sessions()
            .into_iter()
            .filter(|session| session.state.is_active() && session.target.is_some())
            .filter(|session| !self.session_operations.contains_key(&session.id))
            .filter(|session| self.transition_kind(&session.id).is_none())
            .map(|session| {
                (
                    self.git_probe_at.get(&session.id).copied(),
                    session.id.clone(),
                )
            })
            .filter(|(probed, _)| probed.is_none_or(|probed| now.duration_since(probed) >= REFRESH))
            .collect::<Vec<_>>();
        due.sort();
        // Only what the host will read this pass counts as read. The rest
        // stay due, so the next pass reaches them.
        due.truncate(limit);
        for (_, id) in &due {
            self.git_probe_at.insert(id.clone(), now);
        }
        due.into_iter().map(|(_, id)| id).collect()
    }

    /// The branch text a session row carries, when its checkout has been
    /// read and is a repository.
    pub(crate) fn git_row_text(&self, session_id: &str) -> Option<String> {
        self.git_status
            .get(session_id)
            .and_then(|status| status.as_ref().ok())
            .map(|status| status.row_text_with(mj_chat::theme::ascii()))
            .filter(|text| !text.is_empty())
    }

    /// Clears the unread marker on the sessions the active workspace lists,
    /// which is exactly the set its tab badge counts. A session with no row
    /// contributes to no badge, so leaving it unread hides nothing.
    pub(crate) fn mark_all_read(&mut self) -> DashboardAction {
        let listed = self
            .ordered_sessions_unfiltered()
            .into_iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        let mut receipts = Vec::new();
        for session_id in listed {
            let Some(detail) = self.session_details.get_mut(&session_id) else {
                continue;
            };
            if !detail.has_unread() {
                continue;
            }
            let Some(through) = detail.materialized_applied_event_ordinal else {
                continue;
            };
            let Some(session) = self.state.sessions.get_mut(&session_id) else {
                continue;
            };
            if through > session.viewed_through_event_ordinal {
                session.viewed_through_event_ordinal = through;
                detail.clear_unread();
                receipts.push((session_id, through));
            }
        }
        if receipts.is_empty() {
            self.set_notice("No unread sessions in this workspace.");
            DashboardAction::None
        } else {
            self.set_notice("Marked this workspace read; questions and failures stay flagged.");
            DashboardAction::MarkAllRead { receipts }
        }
    }

    pub(crate) fn compatible_profiles(&self, session_id: &str) -> Vec<(&String, HarnessKind)> {
        if self.session_record(session_id).is_none() {
            return Vec::new();
        }
        self.config
            .profiles
            .iter()
            .filter(|(_, profile)| profile.enabled)
            .map(|(id, profile)| (id, profile.kind))
            .collect()
    }

    /// One row of the profile picker tables: the warning marker, the profile
    /// id, the harness, and the two quota percentages remaining. The picker
    /// pads the cells into aligned columns and draws the marker's footnote
    /// below the table.
    pub(crate) fn profile_choice(&self, id: &str, harness: HarnessKind) -> PickerChoice {
        let (weekly, five_hour) = self.profile_quota_cells(id);
        PickerChoice::table(vec![
            guardian_warning_marker(harness),
            PickerCell::text(id),
            PickerCell::text(harness.display_name()),
            weekly,
            five_hour,
        ])
    }

    /// The WEEKLY and 5H cells of a profile picker row, as percentages
    /// remaining coloured by how much headroom they leave. The five-hour cell
    /// stays blank whenever there is no five-hour figure worth reading: a
    /// profile that is refreshing, failing, usage-priced, or out of weekly
    /// quota altogether.
    fn profile_quota_cells(&self, id: &str) -> (PickerCell, PickerCell) {
        let plain = |text: &str| (PickerCell::text(text), PickerCell::blank());
        if self.quota_refreshing.contains(id) {
            return plain("refreshing");
        }
        let Some(quota) = self.quotas.get(id) else {
            return plain("refreshing");
        };
        if quota.error.is_some() {
            return plain(
                &quota
                    .error_label()
                    .unwrap_or_else(|| "unavailable".to_string()),
            );
        }
        if quota.is_usage_priced() {
            return plain(API_LABEL);
        }
        let Some(weekly) = quota.weekly_window().and_then(quota_remaining_percent) else {
            return plain("unavailable");
        };
        let five_hour = if weekly_quota_exhausted(quota) {
            None
        } else {
            quota.five_hour_window().and_then(quota_remaining_percent)
        };
        let percent = |value: u8| {
            PickerCell::styled(
                format!("{value}%"),
                Style::default().fg(headroom_color(value)),
            )
        };
        (
            percent(weekly),
            five_hour.map_or_else(PickerCell::blank, percent),
        )
    }

    /// The selected session, if its target template creates a container.
    pub(crate) fn selected_container_session(&self) -> Option<&SessionRecord> {
        let session = self.command_session()?;
        matches!(
            self.config.targets.get(&session.target_template_id)?,
            HelTargetTemplate::LocalPodman { .. }
                | HelTargetTemplate::LocalDocker { .. }
                | HelTargetTemplate::AppleContainer { .. }
                | HelTargetTemplate::SshPodman { .. }
                | HelTargetTemplate::SshDocker { .. }
        )
        .then_some(session)
    }

    pub(crate) fn config_is_empty(&self) -> bool {
        self.config.enabled_profiles().next().is_none() || self.config.targets.is_empty()
    }

    /// Identity for supervised launch checks; cancellation invalidates late replies.
    pub fn session_preflight_generation(&self) -> u64 {
        self.session_preflight_generation
    }

    /// Invalidates an in-flight session preflight while keeping its modal open.
    /// Selection changes use this so a late result cannot describe a different
    /// target or repository bundle.
    pub(crate) fn invalidate_session_preflight(&mut self) {
        self.session_preflight_generation = self.session_preflight_generation.wrapping_add(1);
    }

    pub fn cancel_modal(&mut self) {
        if self.review_settings_discovery_active() {
            self.review_settings_generation = self.review_settings_generation.wrapping_add(1);
        }
        self.session_preflight_generation = self.session_preflight_generation.wrapping_add(1);
        if matches!(self.mode, Mode::WorkspaceManager(_)) {
            self.workspace_management_generation =
                self.workspace_management_generation.wrapping_add(1);
        }
        self.mode = Mode::Dashboard;
        self.rebuild_resume_rows();
    }

    pub(crate) fn focus_len_for(&self, focus: Focus) -> usize {
        match focus {
            Focus::Sessions => self.visible_session_indices().len(),
            Focus::Targets => self.capacity_details.len(),
            Focus::Quota => self.config.enabled_profiles().count(),
            Focus::Workspaces | Focus::Prompt => 0,
        }
    }

    /// Moves the focused list's selection to `index`, counted among the rows
    /// currently on screen.
    pub(crate) fn set_selection_for(&mut self, focus: Focus, index: usize) -> bool {
        self.scroll_lookahead.set(None);
        if focus == Focus::Sessions {
            self.set_session_action_focus(None);
        }
        match focus {
            Focus::Sessions => {
                let sessions = self.ordered_sessions();
                let next = self
                    .visible_session_indices()
                    .get(index)
                    .and_then(|session| sessions.get(*session))
                    .map(|session| session.id.clone());
                if let Some(next) = next
                    && self.selected_session_id() != Some(next.as_str())
                {
                    self.select_active_session(&next);
                    true
                } else {
                    false
                }
            }
            Focus::Targets => {
                if self.capacity_index != index {
                    self.capacity_index = index;
                    true
                } else {
                    false
                }
            }
            Focus::Quota => {
                if self.quota_index != index {
                    self.quota_index = index;
                    true
                } else {
                    false
                }
            }
            Focus::Workspaces | Focus::Prompt => false,
        }
    }

    pub(crate) fn selection_for(&self, focus: Focus) -> usize {
        match focus {
            Focus::Sessions => self.selected_visible_index().unwrap_or(0),
            Focus::Targets => self.capacity_index,
            Focus::Quota => self.quota_index,
            Focus::Workspaces | Focus::Prompt => 0,
        }
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        self.scroll_selection_for(self.focus, delta);
    }

    pub(crate) fn scroll_selection_for(&mut self, focus: Focus, delta: isize) {
        let len = self.focus_len_for(focus);
        if len == 0 {
            self.set_selection_for(focus, 0);
            return;
        }
        if focus == Focus::Sessions && self.selected_visible_index().is_none() {
            self.set_selection_for(focus, if delta < 0 { len - 1 } else { 0 });
            return;
        }
        let mut index = self.selection_for(focus).min(len.saturating_sub(1));
        let previous = index;
        move_index(&mut index, len, delta);
        self.set_selection_for(focus, index);
        if index != previous {
            self.scroll_lookahead.set(Some((
                focus,
                if delta < 0 {
                    SelectionDirection::Up
                } else {
                    SelectionDirection::Down
                },
            )));
        }
    }

    /// Clamp auxiliary list positions. Session selection belongs to navigation
    /// and is never reassigned by row filtering or refresh.
    pub(crate) fn clamp_selections(&mut self) {
        let project_keys = self.project_keys();
        self.collapsed_project_keys
            .retain(|key| project_keys.contains(key));
        self.quota_index = self
            .quota_index
            .min(self.config.enabled_profiles().count().saturating_sub(1));
        self.capacity_index = self
            .capacity_index
            .min(self.capacity_details.len().saturating_sub(1));
    }
}

#[cfg(test)]
mod verdict_tests {
    use super::*;

    fn level(detail: &SessionDetail) -> AttentionLevel {
        attention_level(
            Some(detail),
            None,
            SessionState::Running,
            false,
            false,
            false,
        )
    }

    #[test]
    fn awaiting_input_demands_attention_until_a_new_turn_starts() {
        let mut detail = SessionDetail {
            awaiting_input: true,
            unread_agent_messages: 1,
            ..Default::default()
        };
        assert_eq!(level(&detail), AttentionLevel::Waiting);
        detail.current_turn_started_at = Some(1);
        assert_eq!(level(&detail), AttentionLevel::Working);
    }

    #[test]
    fn expected_continuation_outranks_unread_output() {
        let mut detail = SessionDetail {
            unread_agent_messages: 1,
            ..Default::default()
        };
        detail.activity.state = Some(mj_core::activity::ActivityState::Expecting { since_ms: 1 });
        assert_eq!(level(&detail), AttentionLevel::Working);
    }
}

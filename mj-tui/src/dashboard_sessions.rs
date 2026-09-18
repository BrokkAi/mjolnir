use super::*;

use ratatui::style::Style;

use mj_client::quota::API_LABEL;

use crate::render::{headroom_color, quota_remaining_percent, weekly_quota_exhausted};

/// How much a session needs a person right now, from least to most.
///
/// The order is the order a person wants to be interrupted in: a failure
/// first, then a session nothing can be learned about because its worker is
/// unreachable, then a question the agent cannot proceed without, then a
/// finished answer nobody has read, then work in progress, then idle, then
/// anything stopped or still starting. It is one scale for the row symbol,
/// the band colour, the priority sort, the attention queue, and the badges,
/// so they can never disagree.
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
    if !detail.activity.is_idle(detail.current_turn_started_at) {
        AttentionLevel::Working
    } else if detail.has_unread() {
        AttentionLevel::Unread
    } else {
        AttentionLevel::Idle
    }
}

impl DashboardState {
    pub(crate) fn selected_session(&self) -> Option<&SessionRecord> {
        let selected = self.selected_session_id.as_deref()?;
        self.ordered_sessions()
            .into_iter()
            .find(|session| session.id == selected)
    }

    /// The live sessions the Sessions pane is showing, as indices into
    /// [`Self::ordered_sessions`].
    pub(crate) fn visible_session_indices(&self) -> Vec<usize> {
        self.sessions_rows()
            .into_iter()
            .filter_map(|row| match row {
                SessionsRow::Session { index, .. } => Some(index),
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
        let sessions = self.ordered_sessions();
        self.expanded_sessions_rows(&sessions)
    }

    pub(crate) fn expanded_sessions_rows(&self, sessions: &[&SessionRecord]) -> Vec<SessionsRow> {
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
        let numbered = self.project_keys().len() > 1;
        let grouped = self.config.advanced.session_order == SessionOrder::Project;
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
        let selected = self.selected_session_id.as_deref()?;
        let sessions = self.ordered_sessions();
        self.visible_session_indices()
            .into_iter()
            .position(|index| sessions.get(index).is_some_and(|s| s.id == selected))
    }

    /// Sessions visible in the selected workspace, grouped by project and
    /// ordered by creation. Stopped sessions are included only when their
    /// advanced display setting is enabled; in-flight transitions remain
    /// visible regardless. The controller may feed all workspaces into one
    /// state snapshot; the tab is the local view filter.
    pub(crate) fn ordered_sessions(&self) -> Vec<&SessionRecord> {
        let sessions = self.ordered_sessions_unfiltered();
        let Some(filter) = self.sessions_filter.as_ref() else {
            return sessions;
        };
        sessions
            .into_iter()
            .filter(|session| self.session_matches_filter(session, filter))
            .collect()
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
        let query = filter.query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        let source = self.project_source(session);
        [
            session.display_title().to_lowercase(),
            session.id.to_lowercase(),
            source.short.to_lowercase(),
            source.full.to_lowercase(),
            session.last_profile.to_lowercase(),
            session.target_template_id.to_lowercase(),
        ]
        .iter()
        .any(|field| field.contains(&query))
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
            parts.push(format!("/{}", filter.query));
        }
        if let Some(state) = filter.state {
            parts.push(state.label().to_owned());
        }
        parts.join(" · ")
    }

    /// Answers a key for the Sessions filter, or `None` when the filter does
    /// not claim it. While editing, printable keys are text and the arrows
    /// still move the selection; `Enter` keeps the filter and returns the
    /// letters to the pane; `Esc` clears the text, and a second `Esc` clears
    /// the state filter too. When not editing, the state letters narrow the
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
                        self.sessions_filter = None;
                    }
                }
                KeyCode::Esc => {
                    if filter.query.is_empty() {
                        self.sessions_filter = None;
                    } else {
                        filter.query.clear();
                    }
                }
                KeyCode::Backspace => {
                    filter.query.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    filter.query.clear();
                }
                KeyCode::Char(character)
                    if !key.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    ) =>
                {
                    filter.query.push(character);
                }
                _ => return None,
            }
            self.clamp_selections();
            return Some(());
        }
        if !plain {
            return None;
        }
        match key.code {
            KeyCode::Esc if self.sessions_filter.is_some() => {
                self.sessions_filter = None;
            }
            KeyCode::Char(letter) if SessionStateFilter::from_letter(letter).is_some() => {
                let state = SessionStateFilter::from_letter(letter)?;
                match (state, self.sessions_filter.as_mut()) {
                    (None, None) => return None,
                    (None, Some(filter)) => {
                        filter.state = None;
                        if filter.query.is_empty() {
                            self.sessions_filter = None;
                        }
                    }
                    (Some(state), Some(filter)) => filter.state = Some(state),
                    (Some(state), None) => {
                        self.sessions_filter = Some(SessionsFilter {
                            query: String::new(),
                            state: Some(state),
                            editing: false,
                        });
                    }
                }
            }
            _ => return None,
        }
        self.clamp_selections();
        Some(())
    }

    /// Whether the Sessions pane lists `session` as a top-level row of
    /// `workspace_id`: live, mid-transition, or stopped when stopped sessions
    /// are shown. Terminal failures such as a lost or data-loss session have
    /// no row, so the badges and the attention queue must not count them
    /// either; they are reachable only through the resume dialog.
    fn is_listed_top_level_session(&self, session: &SessionRecord, workspace_id: &str) -> bool {
        session.workspace_id == workspace_id
            && !self.state.subagents.contains_key(&session.id)
            && (session.state.is_active()
                || self.transition_kind(&session.id).is_some()
                || (self.config.advanced.show_stopped_sessions
                    && session.state == SessionState::Stopped))
    }

    fn ordered_sessions_unfiltered(&self) -> Vec<&SessionRecord> {
        if let Some(parent_id) = self.subagent_parent_id.as_deref() {
            let mut children = self
                .state
                .subagents
                .values()
                .filter(|record| record.parent_session_id == parent_id)
                .filter_map(|record| self.state.sessions.get(&record.child_session_id))
                .collect::<Vec<_>>();
            children.sort_by_cached_key(|session| session.creation_order_key());
            return children;
        }
        let Some(active_workspace_id) = self.active_workspace_id.as_deref() else {
            return Vec::new();
        };
        let active = self
            .state
            .sessions
            .values()
            .filter(|session| self.is_listed_top_level_session(session, active_workspace_id))
            .collect::<Vec<_>>();
        let priority = self.config.advanced.session_order == SessionOrder::Priority;
        let inputs = active
            .iter()
            .map(|session| {
                let source = self.project_source(session);
                (
                    session.id.clone(),
                    if priority {
                        format!(
                            "{:?}/{}",
                            self.attention_level(&session.id),
                            self.last_activity_ms(&session.id)
                        )
                    } else {
                        session.created_at.clone()
                    },
                    source.key,
                    source.short,
                    source.full,
                )
            })
            .collect::<Vec<_>>();
        let mut cache = self.session_order_cache.borrow_mut();
        if cache.inputs == inputs {
            return cache
                .ids
                .iter()
                .filter_map(|id| self.state.sessions.get(id))
                .collect();
        }
        let mut active = active;
        if self.config.advanced.session_order == SessionOrder::Priority {
            active.sort_by_cached_key(|session| {
                (
                    std::cmp::Reverse(self.attention_level(&session.id)),
                    std::cmp::Reverse(self.last_activity_ms(&session.id)),
                    session.creation_order_key(),
                )
            });
            cache.inputs = inputs;
            cache.ids = active.iter().map(|session| session.id.clone()).collect();
            return active;
        }
        active.sort_by_cached_key(|session| session.creation_order_key());
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
        let ordered = groups.into_iter().flatten().collect::<Vec<_>>();
        cache.inputs = inputs;
        cache.ids = ordered.iter().map(|session| session.id.clone()).collect();
        ordered
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
        let mut keys = Vec::new();
        for session in self.ordered_sessions() {
            let key = self.project_source(session).key;
            if keys.last() != Some(&key) {
                keys.push(key);
            }
        }
        keys
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
    pub fn attention_level(&self, session_id: &str) -> AttentionLevel {
        let Some(session) = self.state.sessions.get(session_id) else {
            return AttentionLevel::Inactive;
        };
        attention_level(
            self.session_details.get(session_id),
            self.session_review(session_id),
            session.state,
            self.unreachable_sessions.contains(session_id),
            self.session_operations.contains_key(session_id)
                || self.transition_kind(session_id).is_some(),
            self.transition_failure_kind(session_id).is_some(),
        )
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
            .state
            .sessions
            .values()
            .filter(|session| self.is_listed_top_level_session(session, &session.workspace_id))
            .filter_map(|session| {
                let level = self.attention_level(&session.id);
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

    /// The most urgent level among `sessions` and how many of them need a
    /// person at all, for a badge. `None` when none of them do.
    fn attention_summary<'a>(
        &self,
        sessions: impl IntoIterator<Item = &'a SessionRecord>,
    ) -> Option<(AttentionLevel, usize)> {
        sessions
            .into_iter()
            .filter_map(|session| {
                let level = self.attention_level(&session.id);
                level.needs_person().then_some(level)
            })
            .fold(None, |summary, level| match summary {
                Some((top, count)) => Some((top.max(level), count + 1)),
                None => Some((level, 1)),
            })
    }

    /// The badge for the sessions of one workspace, for its tab.
    pub(crate) fn workspace_attention_summary(
        &self,
        workspace_id: &str,
    ) -> Option<(AttentionLevel, usize)> {
        self.attention_summary(
            self.state
                .sessions
                .values()
                .filter(|session| self.is_listed_top_level_session(session, workspace_id)),
        )
    }

    /// The badge for the visible sessions of one project, for a folded
    /// heading.
    pub(crate) fn project_attention_summary(
        &self,
        project_key: &str,
    ) -> Option<(AttentionLevel, usize)> {
        self.attention_summary(
            self.ordered_sessions()
                .into_iter()
                .filter(|session| self.project_source(session).key == project_key),
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
        Some((top, queue.len()))
    }

    /// Moves to the next (`1`) or previous (`-1`) session in the attention
    /// queue, counting from the selected session when it is in the queue and
    /// from the top otherwise.
    ///
    /// A session in another workspace is reached by recording it as that
    /// workspace's selection and asking the host to switch: the host restores
    /// the selection when the tab changes and opens its conversation, exactly
    /// as it does for a tab the person clicks.
    pub(crate) fn step_attention(&mut self, delta: isize) -> DashboardAction {
        let queue = self.attention_queue();
        if queue.is_empty() {
            self.set_notice("Nothing is waiting for you.");
            return DashboardAction::None;
        }
        let position = self
            .selected_session_id
            .as_deref()
            .and_then(|selected| queue.iter().position(|entry| entry.session_id == selected));
        let target = match position {
            Some(position) => {
                let len = queue.len() as isize;
                queue[(position as isize + delta).rem_euclid(len) as usize].clone()
            }
            None if delta < 0 => queue[queue.len() - 1].clone(),
            None => queue[0].clone(),
        };
        if self.subagent_parent_id.is_some() {
            self.close_subagent_workspace();
        }
        if self.active_workspace_id.as_deref() != Some(target.workspace_id.as_str()) {
            let view = self
                .workspace_views
                .entry(target.workspace_id.clone())
                .or_insert_with(|| WorkspaceViewState {
                    selected_session_id: None,
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
            view.selected_session_id = Some(target.session_id.clone());
            view.focus = Focus::Prompt;
            return DashboardAction::SelectWorkspace {
                workspace_id: target.workspace_id,
            };
        }
        if let Some(session) = self.state.sessions.get(&target.session_id) {
            let key = self.project_source(session).key;
            self.collapsed_project_keys.remove(&key);
        }
        // A filter that hides the session the person asked for is no longer
        // what they want.
        if !self
            .ordered_sessions()
            .iter()
            .any(|session| session.id == target.session_id)
        {
            self.sessions_filter = None;
        }
        self.select_active_session(&target.session_id);
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
    /// minute, oldest reading first. The host reads a few of these per tick.
    pub fn git_probe_candidates(&mut self, now: Instant) -> Vec<String> {
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

    pub(crate) fn mark_all_read(&mut self) -> DashboardAction {
        let mut receipts = Vec::new();
        for (session_id, detail) in &mut self.session_details {
            if !detail.has_unread() {
                continue;
            }
            let Some(through) = detail.materialized_applied_event_ordinal else {
                continue;
            };
            let Some(session) = self.state.sessions.get_mut(session_id) else {
                continue;
            };
            if through > session.viewed_through_event_ordinal {
                session.viewed_through_event_ordinal = through;
                detail.clear_unread();
                receipts.push((session_id.clone(), through));
            }
        }
        if receipts.is_empty() {
            self.set_notice("No unread sessions.");
            DashboardAction::None
        } else {
            self.set_notice("Marked all sessions read; questions and failures stay flagged.");
            DashboardAction::MarkAllRead { receipts }
        }
    }

    pub(crate) fn compatible_profiles(&self, session_id: &str) -> Vec<(&String, HarnessKind)> {
        if !self.state.sessions.contains_key(session_id) {
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
        let session = self.selected_session()?;
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
                if self.selected_session_id != next {
                    self.selected_session_id = next;
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

    pub(crate) fn clamp_selections(&mut self) {
        // The selection is anchored by id, so it survives the list changing
        // under it; it only moves when the session it named stopped being on
        // screen.
        let sessions = self.ordered_sessions();
        let visible = self
            .visible_session_indices()
            .into_iter()
            .filter_map(|index| sessions.get(index).map(|session| session.id.clone()))
            .collect::<Vec<_>>();
        if !self
            .selected_session_id
            .as_ref()
            .is_some_and(|id| visible.contains(id))
        {
            self.selected_session_id = visible.into_iter().next();
        }
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

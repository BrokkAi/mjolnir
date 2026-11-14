//! What the dashboard derives about its sessions: each session's attention
//! level, the sessions the Sessions pane can list, and the pane's order,
//! project groups, and rows.
//!
//! Rows, badges, the attention queue, notifications, and key handling all
//! read these facts, many times per frame. They are derived once per change
//! of the inputs they come from, and every reader asks the dashboard for the
//! derived result rather than recomputing it. Each input is a [`Tracked`]
//! field, so any change to it is also what marks the facts stale: there is
//! no separate invalidation for a new mutation to forget.

use std::cell::Ref;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::dashboard_sessions::presentation_attention;

/// Revisions come from one counter, so a replaced value never repeats a
/// revision an earlier value had.
static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

fn next_revision() -> u64 {
    NEXT_REVISION.fetch_add(1, Ordering::Relaxed)
}

/// A dashboard input that the session facts are derived from.
///
/// Reading goes through [`Deref`] and costs nothing. Every mutable access
/// goes through [`DerefMut`], which takes a new revision; the derived facts
/// remember the revisions they were made from and are rebuilt when one
/// differs. A mutable access that changes nothing only costs a rebuild.
pub(crate) struct Tracked<T> {
    value: T,
    revision: u64,
}

impl<T> Tracked<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            value,
            revision: next_revision(),
        }
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
}

impl<T: Default> Default for Tracked<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> From<T> for Tracked<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T> Deref for Tracked<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Tracked<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.revision = next_revision();
        &mut self.value
    }
}

impl<T: PartialEq> PartialEq<T> for Tracked<T> {
    fn eq(&self, other: &T) -> bool {
        self.value == *other
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Tracked<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(formatter)
    }
}

impl<'a, T> IntoIterator for &'a Tracked<T>
where
    &'a T: IntoIterator,
{
    type Item = <&'a T as IntoIterator>::Item;
    type IntoIter = <&'a T as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        (&self.value).into_iter()
    }
}

/// Facts about each session that do not depend on what the pane shows.
#[derive(Default)]
pub(crate) struct SessionFacts {
    /// Revisions of the inputs these facts were derived from; `None` before
    /// the first derivation.
    inputs: Option<[u64; 5]>,
    /// Served attention with this terminal's unread receipt applied.
    own: HashMap<String, AttentionLevel>,
    /// For each parent, its first active Mjolnir sub-agent that is waiting
    /// on a question.
    waiting_child: HashMap<String, String>,
    /// Live records, and records an operation keeps on screen, by id.
    listed: Vec<String>,
    /// How many times the facts were derived, for tests to count.
    #[cfg(test)]
    derivations: usize,
}

impl SessionFacts {
    fn derive(dashboard: &DashboardState, inputs: [u64; 5]) -> Self {
        let mut index = dashboard.row_index.borrow_mut();
        index.synchronize(&dashboard.state);
        let own = dashboard
            .state
            .sessions
            .keys()
            .map(|id| (id.clone(), presentation_attention(dashboard, id)))
            .collect::<HashMap<_, _>>();
        let holds_form = |child: &str| {
            own.get(child) == Some(&AttentionLevel::Waiting)
                && dashboard.state.sessions.contains_key(child)
                && dashboard
                    .session_details
                    .get(child)
                    .is_some_and(|detail| !detail.pending_elicitations.is_empty())
        };
        // A descendant holding a form, at any depth, so a child's reviewer
        // reaches the top-level row too. Validation rejects
        // ownership cycles; `visited` keeps a malformed state finite anyway.
        let waiting_child = index
            .active_children
            .keys()
            .filter_map(|parent| {
                let mut visited = std::collections::HashSet::new();
                let mut stack = vec![parent.as_str()];
                while let Some(session) = stack.pop() {
                    if !visited.insert(session) {
                        continue;
                    }
                    let Some(children) = index.active_children.get(session) else {
                        continue;
                    };
                    if let Some(child) = children.iter().find(|child| holds_form(child)) {
                        return Some((parent.clone(), child.clone()));
                    }
                    stack.extend(children.iter().rev().map(String::as_str));
                }
                None
            })
            .collect();
        let mut listed = index.live.clone();
        listed.extend(
            own.iter()
                .filter(|(_, level)| level.needs_person())
                .map(|(id, _)| id.clone()),
        );
        listed.extend(dashboard.session_operations.keys().cloned());
        let listed = listed
            .into_iter()
            .filter(|id| dashboard.state.sessions.contains_key(id))
            .collect();
        Self {
            inputs: Some(inputs),
            own,
            waiting_child,
            listed,
            #[cfg(test)]
            derivations: 0,
        }
    }

    pub(crate) fn attention_level(&self, session_id: &str) -> AttentionLevel {
        self.own
            .get(session_id)
            .copied()
            .unwrap_or(AttentionLevel::Inactive)
    }

    pub(crate) fn waiting_child(&self, parent_id: &str) -> Option<&str> {
        self.waiting_child.get(parent_id).map(String::as_str)
    }

    pub(crate) fn listed(&self) -> &[String] {
        &self.listed
    }
}

/// A session's CPU together with its sub-agents', for display.
///
/// A parent's figure is its own CPU plus that of every descendant that has
/// a live worker, so a parent that waits on busy children still shows load.
/// Recent and hourly shares are summed (a share can exceed 100% across
/// processes). If some member (the session or a live descendant) has no
/// measurement, the sum is a lower bound and `partial` is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) struct CpuRollup {
    pub(crate) recent_permille: u32,
    pub(crate) hourly_permille: u32,
    /// The session itself plus its live descendants.
    pub(crate) members: usize,
    pub(crate) measured: usize,
}

impl CpuRollup {
    pub(crate) fn is_partial(&self) -> bool {
        self.measured < self.members
    }

    pub(crate) fn has_descendants(&self) -> bool {
        self.members > 1
    }
}

/// The rolled-up figure a row shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CpuShare {
    pub(crate) permille: u16,
    pub(crate) partial: bool,
    pub(crate) included: bool,
}

impl CpuShare {
    /// The row text: the share, then `+` when it is a lower bound.
    pub(crate) fn label(&self) -> String {
        if self.included {
            return "included in parent".into();
        }
        let marker = if self.partial { "+" } else { "" };
        format!(
            "{}{marker}",
            mj_client::usage_format::format_cpu_permille(self.permille)
        )
    }
}

pub(crate) fn clamp_permille(permille: u32) -> u16 {
    permille.min(u32::from(u16::MAX)) as u16
}

/// Every session's [`CpuRollup`] and its live sub-agents, derived when a
/// CPU sample or the session tree changes. Rows, the live CPU report, and
/// the redraw check all read this one result.
#[derive(Default)]
pub(crate) struct CpuRollups {
    inputs: Option<[u64; 2]>,
    by_session: HashMap<String, CpuRollup>,
    samples: HashMap<String, mj_core::cpu_usage::SessionCpuUsage>,
    owners: HashMap<String, String>,
    /// Live sub-agents of each parent, in id order.
    children: HashMap<String, Vec<String>>,
    #[cfg(test)]
    derivations: usize,
}

impl CpuRollups {
    fn derive(dashboard: &DashboardState, inputs: [u64; 2]) -> Self {
        let index = dashboard.synchronized_row_index();
        let children: HashMap<String, Vec<String>> = index
            .active_children
            .iter()
            .map(|(parent, ids)| (parent.clone(), ids.iter().cloned().collect()))
            .collect();
        let mut samples: HashMap<_, _> = dashboard
            .session_cpu
            .iter()
            .filter_map(|(id, view)| match view {
                mj_client::runtime_feed::SessionCpuView::Measured { usage } => {
                    Some((id.clone(), *usage))
                }
                _ => None,
            })
            .collect();
        // A physical container has one contribution even while workers upgrade or move.
        let mut groups = std::collections::BTreeMap::new();
        for (id, session) in dashboard.state.sessions.iter().filter(|(_, session)| {
            session.state.is_active()
                && !matches!(
                    session.state,
                    mj_core::state::SessionState::Parked | mj_core::state::SessionState::Error
                )
        }) {
            if let Some(scope) = session
                .target
                .as_ref()
                .and_then(|target| target.cpu_container_scope())
            {
                groups
                    .entry(scope)
                    .or_insert_with(Vec::new)
                    .push(id.clone());
            }
        }
        let mut owners: HashMap<String, String> = dashboard
            .state
            .sessions
            .keys()
            .map(|id| (id.clone(), id.clone()))
            .collect();
        for ids in groups.values_mut() {
            ids.sort_by_key(|id| {
                (
                    !dashboard
                        .state
                        .sessions
                        .get(id)
                        .and_then(|session| session.target.as_ref())
                        .is_some_and(|target| target.owns_container()),
                    id.clone(),
                )
            });
            let canonical = &ids[0];
            let sample = ids
                .iter()
                .filter_map(|id| samples.get(id))
                .find(|sample| sample.scope == mj_core::cpu_usage::CpuSampleScope::Container)
                .copied();
            if let Some(sample) = sample {
                for id in ids.iter() {
                    owners.insert(id.clone(), canonical.clone());
                }
                samples.insert(canonical.clone(), sample);
            }
        }
        let mut result = Self {
            inputs: Some(inputs),
            samples,
            owners,
            children,
            ..Self::default()
        };
        let mut by_session = HashMap::new();
        for id in dashboard.state.sessions.keys() {
            let mut members = Vec::new();
            let mut seen = std::collections::HashSet::new();
            let mut pending = vec![id.as_str()];
            while let Some(member) = pending.pop() {
                if !seen.insert(member) {
                    continue;
                }
                members.push(member);
                pending.extend(
                    result
                        .children
                        .get(member)
                        .into_iter()
                        .flatten()
                        .map(String::as_str),
                );
            }
            by_session.insert(id.clone(), result.total(members));
        }
        result.by_session = by_session;
        result
    }

    fn total<'a>(&self, members: impl IntoIterator<Item = &'a str>) -> CpuRollup {
        let mut rollup = CpuRollup::default();
        let mut scopes = std::collections::HashSet::new();
        for member in members {
            rollup.members += 1;
            let owner = self
                .owners
                .get(member)
                .map(String::as_str)
                .unwrap_or(member);
            if let Some(usage) = self.samples.get(owner) {
                rollup.measured += 1;
                if scopes.insert(owner) {
                    rollup.recent_permille += u32::from(usage.recent_permille);
                    rollup.hourly_permille += u32::from(usage.hourly_permille);
                }
            }
        }
        rollup
    }
}

/// What the Sessions pane shows, in order.
#[derive(Default)]
pub(crate) struct SessionOrder {
    key: Option<OrderKey>,
    /// The sessions in `ordered_sessions()` order.
    ids: Vec<String>,
    positions: HashMap<String, usize>,
    /// Each ordered session's project key.
    projects: Vec<String>,
    /// Project keys in the order their groups appear.
    project_keys: Vec<String>,
    rows: Vec<SessionsRow>,
    /// How many sessions the Sessions filter holds back.
    hidden: usize,
    /// Each ordered session's target label.
    targets: Vec<String>,
    /// How many times the order was derived, for tests to count.
    #[cfg(test)]
    derivations: usize,
}

#[derive(Clone, PartialEq, Eq)]
struct OrderKey {
    inputs: [u64; 14],
    selected: Option<String>,
}

impl SessionOrder {
    fn derive(dashboard: &DashboardState, key: OrderKey) -> Self {
        let (sessions, hidden) = dashboard.derive_ordered_sessions();
        let projects = sessions
            .iter()
            .map(|session| dashboard.project_source(session).key)
            .collect::<Vec<_>>();
        let mut project_keys: Vec<String> = Vec::new();
        for project in &projects {
            if project_keys.last() != Some(project) {
                project_keys.push(project.clone());
            }
        }
        let rows = dashboard.derive_sessions_rows(&sessions, project_keys.len() > 1);
        let targets = crate::render::session_display_targets(dashboard, &sessions, &projects);
        let ids = sessions
            .iter()
            .map(|session| session.id.clone())
            .collect::<Vec<_>>();
        let positions = ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.clone(), index))
            .collect();
        Self {
            key: Some(key),
            ids,
            positions,
            projects,
            project_keys,
            rows,
            hidden,
            targets,
            #[cfg(test)]
            derivations: 0,
        }
    }

    pub(crate) fn ids(&self) -> &[String] {
        &self.ids
    }

    pub(crate) fn position(&self, session_id: &str) -> Option<usize> {
        self.positions.get(session_id).copied()
    }

    pub(crate) fn project(&self, index: usize) -> Option<&str> {
        self.projects.get(index).map(String::as_str)
    }

    pub(crate) fn project_keys(&self) -> &[String] {
        &self.project_keys
    }

    pub(crate) fn rows(&self) -> &[SessionsRow] {
        &self.rows
    }

    pub(crate) fn hidden(&self) -> usize {
        self.hidden
    }

    pub(crate) fn targets(&self) -> &[String] {
        &self.targets
    }
}

impl DashboardState {
    fn session_facts_inputs(&self) -> [u64; 5] {
        [
            self.state.revision(),
            self.session_details.revision(),
            self.session_reviews.revision(),
            self.served_attention.revision(),
            self.session_operations.revision(),
        ]
    }

    /// The session facts for the dashboard's current inputs, derived again
    /// only when one of those inputs changed since the last read.
    pub(crate) fn session_facts(&self) -> Ref<'_, SessionFacts> {
        let inputs = self.session_facts_inputs();
        if self.session_facts.borrow().inputs != Some(inputs) {
            let facts = SessionFacts::derive(self, inputs);
            let mut slot = self.session_facts.borrow_mut();
            #[cfg(test)]
            let facts = SessionFacts {
                derivations: slot.derivations + 1,
                ..facts
            };
            *slot = facts;
        }
        self.session_facts.borrow()
    }

    fn cpu_rollups_inputs(&self) -> [u64; 2] {
        [self.state.revision(), self.session_cpu.revision()]
    }

    fn cpu_rollups(&self) -> Ref<'_, CpuRollups> {
        let inputs = self.cpu_rollups_inputs();
        if self.cpu_rollups.borrow().inputs != Some(inputs) {
            let rollups = CpuRollups::derive(self, inputs);
            let mut slot = self.cpu_rollups.borrow_mut();
            #[cfg(test)]
            let rollups = CpuRollups {
                derivations: slot.derivations + 1,
                ..rollups
            };
            *slot = rollups;
        }
        self.cpu_rollups.borrow()
    }

    /// A session's CPU with its live sub-agents' included.
    pub(crate) fn cpu_rollup(&self, session_id: &str) -> CpuRollup {
        self.cpu_rollups()
            .by_session
            .get(session_id)
            .copied()
            .unwrap_or_default()
    }

    pub(crate) fn cpu_sample(
        &self,
        session_id: &str,
    ) -> Option<mj_core::cpu_usage::SessionCpuUsage> {
        let rollups = self.cpu_rollups();
        rollups
            .samples
            .get(
                rollups
                    .owners
                    .get(session_id)
                    .map(String::as_str)
                    .unwrap_or(session_id),
            )
            .copied()
    }

    pub(crate) fn cpu_included_in(&self, session_id: &str) -> Option<String> {
        self.cpu_rollups()
            .owners
            .get(session_id)
            .filter(|owner| owner.as_str() != session_id)
            .cloned()
    }

    pub(crate) fn cpu_total<'a>(&self, ids: impl IntoIterator<Item = &'a str>) -> CpuRollup {
        self.cpu_rollups().total(ids)
    }

    /// The figure a Sessions row shows: `None` when nothing in the tree is
    /// measured. The row applies its own display threshold.
    pub(crate) fn cpu_share(&self, session_id: &str) -> Option<CpuShare> {
        let rollup = self.cpu_rollup(session_id);
        (rollup.measured > 0).then(|| CpuShare {
            permille: clamp_permille(rollup.recent_permille),
            partial: rollup.is_partial(),
            included: self.cpu_included_in(session_id).is_some(),
        })
    }

    /// The live sub-agents of a session, in id order.
    pub(crate) fn cpu_children(&self, session_id: &str) -> Vec<String> {
        self.cpu_rollups()
            .children
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    fn session_order_key(&self) -> OrderKey {
        let [state, details, reviews, unreachable, operations] = self.session_facts_inputs();
        OrderKey {
            inputs: [
                state,
                details,
                reviews,
                unreachable,
                operations,
                self.config.revision(),
                self.project_sources.revision(),
                self.native_agents.revision(),
                self.native_by_parent.revision(),
                self.sessions_filter.revision(),
                self.sessions_text.revision(),
                self.active_workspace_id.revision(),
                self.child_workspace.revision(),
                self.collapsed_project_keys.revision(),
            ],
            selected: self.selected_session_id().map(str::to_owned),
        }
    }

    /// The Sessions pane's order and rows for the dashboard's current inputs
    /// and selection, derived again only when one of them changed since the
    /// last read.
    pub(crate) fn session_order(&self) -> Ref<'_, SessionOrder> {
        let key = self.session_order_key();
        if self.session_order.borrow().key.as_ref() != Some(&key) {
            let order = SessionOrder::derive(self, key);
            let mut slot = self.session_order.borrow_mut();
            #[cfg(test)]
            let order = SessionOrder {
                derivations: slot.derivations + 1,
                ..order
            };
            *slot = order;
        }
        self.session_order.borrow()
    }
}

#[cfg(test)]
impl DashboardState {
    /// How many times the session facts and the Sessions order have been
    /// derived, without deriving them.
    pub(crate) fn cpu_rollup_derivations(&self) -> usize {
        self.cpu_rollups.borrow().derivations
    }

    pub(crate) fn session_view_derivations(&self) -> (usize, usize) {
        (
            self.session_facts.borrow().derivations,
            self.session_order.borrow().derivations,
        )
    }
}

//! Read-only selected-workspace data, with owned and cancellable background work.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hel::hel_config::HelConfig;
use hel::hel_state::{HelState, MaterializedSession, ProjectSourceIdentity};
use hel::hel_targets::CancellableProcessExecutor;
use hel_tui::{
    DashboardState, PreparedMaterializedSessionDetail, PreparedMaterializedSessionSummary,
};
use mj_chat::hel_chat::Notices;
use mj_controller::hel_controller::Controller;
use mj_controller::hel_session_manager::ManagedSessionView;
use tokio::task::JoinSet;

use crate::daemon::{self, WorkspaceSnapshot};
use crate::pollers::{RuntimeFeed, RuntimeFeedUpdate, spawn_runtime_feed};
use crate::session_presentation::{apply_lifecycle_display, apply_session_activity};

// Keep slots occupied by cancelled blocking work until it actually exits.
// Slow Git/SSH resolution has its own pool and cannot starve message previews.
static PREPARATION_PERMITS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(4)));
static SOURCE_PERMITS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(2)));

/// One subscription owns its own completion queue. Replacing it when selection
/// changes drops that queue, so old workspace results cannot enter the new view.
pub(super) struct WorkspacePreview {
    workspace_id: String,
    pub dashboard: DashboardState,
    pub metadata: Option<WorkspaceSnapshot>,
    pub loaded: bool,
    controller: Controller,
    feed: Option<RuntimeFeed>,
    tasks: JoinSet<PreparedUpdate>,
    permits: Arc<tokio::sync::Semaphore>,
    cancelled: Arc<AtomicBool>,
    source_generation: u64,
    sources_started: BTreeSet<String>,
    summaries_started: BTreeSet<String>,
    projecting: BTreeSet<String>,
    detailed: BTreeSet<String>,
    runtime_titles: BTreeMap<String, Option<String>>,
    pending: BTreeMap<String, MaterializedSession>,
    lifecycles: BTreeSet<String>,
    feed_error: Option<String>,
    runtime_notices: Notices,
    lifecycle_notices: BTreeMap<String, String>,
    reported_notice_id: Option<u64>,
    errors: BTreeMap<(String, &'static str), String>,
    metadata_in_flight: bool,
    metadata_refresh_at: Instant,
    feed_retry_at: Instant,
}

enum PreparedUpdate {
    Metadata(Result<WorkspaceSnapshot>),
    Summary {
        session_id: String,
        result: Result<Option<(PreparedMaterializedSessionSummary, Option<String>)>>,
    },
    Source {
        session_id: String,
        generation: u64,
        result: Result<ProjectSourceIdentity>,
    },
    Detail {
        session_id: String,
        result: Result<Box<PreparedMaterializedSessionDetail>>,
    },
}

impl Drop for WorkspacePreview {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.tasks.abort_all();
        // RuntimeFeed's drop aborts the daemon long poll. Blocking readers are
        // finite; process-owning source resolution observes cancellation.
    }
}

impl WorkspacePreview {
    pub fn new(workspace_id: String, name: String) -> Self {
        let mut preview = Self::empty(workspace_id, name);
        preview.feed = Some(spawn_runtime_feed(preview.workspace_id.clone()));
        preview.tick();
        preview
    }

    fn empty(workspace_id: String, name: String) -> Self {
        let mut dashboard =
            DashboardState::new(HelConfig::default(), HelState::default(), BTreeMap::new());
        dashboard.set_workspace_name(name);
        Self {
            feed: None,
            workspace_id,
            dashboard,
            metadata: None,
            loaded: false,
            controller: Controller {
                config: HelConfig::default(),
                state: HelState::default(),
            },
            tasks: JoinSet::new(),
            permits: Arc::clone(&PREPARATION_PERMITS),
            cancelled: Arc::new(AtomicBool::new(false)),
            source_generation: 0,
            sources_started: BTreeSet::new(),
            summaries_started: BTreeSet::new(),
            projecting: BTreeSet::new(),
            detailed: BTreeSet::new(),
            runtime_titles: BTreeMap::new(),
            pending: BTreeMap::new(),
            lifecycles: BTreeSet::new(),
            feed_error: None,
            runtime_notices: Notices::default(),
            lifecycle_notices: BTreeMap::new(),
            reported_notice_id: None,
            errors: BTreeMap::new(),
            metadata_in_flight: false,
            metadata_refresh_at: Instant::now(),
            feed_retry_at: Instant::now(),
        }
    }

    pub fn status(&self) -> Option<String> {
        if let Some(error) = self
            .feed_error
            .as_ref()
            .or_else(|| self.errors.values().next())
        {
            let suffix = if self.loaded {
                " (preview may be stale)"
            } else {
                ""
            };
            return Some(format!("{error}{suffix}"));
        }
        if !self.loaded {
            return Some("Loading sessions…".into());
        }
        if !self.projecting.is_empty() || !self.tasks.is_empty() {
            return Some("Loading session details…".into());
        }
        self.runtime_notices.current()
    }

    pub fn session_count(&self) -> Option<usize> {
        self.loaded.then(|| {
            self.controller
                .state
                .sessions
                .values()
                .filter(|session| {
                    session.state.is_active()
                        || matches!(
                            self.dashboard.session_operation_kind(&session.id),
                            Some(
                                hel_tui::SessionOperationKind::Launching
                                    | hel_tui::SessionOperationKind::Resuming
                                    | hel_tui::SessionOperationKind::Importing
                            )
                        )
                })
                .count()
        })
    }

    pub fn metadata_ready(&self) -> bool {
        self.metadata.is_some() && !self.errors.contains_key(&(String::new(), "metadata"))
    }

    pub fn tick(&mut self) {
        let now = Instant::now();
        if self.feed.is_none() && now >= self.feed_retry_at {
            self.feed = Some(spawn_runtime_feed(self.workspace_id.clone()));
        }
        if !self.metadata_in_flight && now >= self.metadata_refresh_at {
            for (id, kind) in self.errors.keys() {
                match *kind {
                    "summary" => {
                        self.summaries_started.remove(id);
                    }
                    "project" => {
                        self.sources_started.remove(id);
                    }
                    _ => {}
                }
            }
            self.errors
                .retain(|(_, kind), _| !matches!(*kind, "summary" | "project"));
            self.prepare_records();
            self.metadata_in_flight = true;
            let workspace_id = self.workspace_id.clone();
            self.tasks.spawn(async move {
                let result = async {
                    daemon::connect_or_start()
                        .await?
                        .snapshot(workspace_id)
                        .await
                }
                .await;
                PreparedUpdate::Metadata(result)
            });
        }
    }

    /// All work after a receive is synchronous application of prepared data;
    /// cancelling this wait for a key or redraw cannot lose an update.
    pub async fn update(&mut self) {
        tokio::select! {
            update = async { self.feed.as_mut().expect("guarded feed").updates.recv().await }, if self.feed.is_some() => {
                match update {
                    Some(update) => self.apply_runtime(update),
                    None => {
                        self.feed = None;
                        self.feed_error = Some("Session feed stopped; reconnecting…".into());
                        self.feed_retry_at = Instant::now() + Duration::from_secs(1);
                    }
                }
            }
            result = self.tasks.join_next(), if !self.tasks.is_empty() => {
                match result {
                    Some(Ok(update)) => self.apply_prepared(update),
                    Some(Err(error)) => {
                        tracing::error!(%error, "workspace preview task failed");
                        self.feed_error = Some(format!("Preview task failed: {error}"));
                    }
                    None => {}
                }
            }
            else => std::future::pending::<()>().await,
        }
    }

    fn apply_runtime(&mut self, update: RuntimeFeedUpdate) {
        match update {
            RuntimeFeedUpdate::Error(error) => self.feed_error = Some(error),
            RuntimeFeedUpdate::Snapshot(snapshot) => {
                self.apply_snapshot(*snapshot);
                self.prepare_records();
            }
            RuntimeFeedUpdate::Session { session_id, view } => {
                self.apply_session(session_id, *view)
            }
        }
    }

    fn apply_snapshot(&mut self, snapshot: daemon::RuntimeSnapshot) {
        self.loaded = true;
        self.feed_error = None;
        let source_changed = self.controller.config != snapshot.config
            || snapshot.records.iter().any(|record| {
                self.controller
                    .state
                    .sessions
                    .get(&record.id)
                    .is_some_and(|previous| {
                        previous.project_directory != record.project_directory
                            || previous.managed_worktree != record.managed_worktree
                            || previous.target_template_id != record.target_template_id
                            || previous.bundle_id != record.bundle_id
                    })
            });
        if source_changed {
            self.source_generation = self.source_generation.wrapping_add(1);
            self.sources_started.clear();
            self.controller.config = snapshot.config.clone();
            self.dashboard.set_config(snapshot.config);
        }
        let active: BTreeSet<_> = snapshot
            .lifecycles
            .iter()
            .map(|op| op.session_id.clone())
            .collect();
        self.controller.state.sessions = snapshot
            .records
            .into_iter()
            .filter(|session| session.state.is_active() || active.contains(&session.id))
            .map(|session| (session.id.clone(), session))
            .collect();
        let records = &self.controller.state.sessions;
        self.errors
            .retain(|(id, _), _| id.is_empty() || records.contains_key(id));
        self.sources_started.retain(|id| records.contains_key(id));
        self.summaries_started.retain(|id| records.contains_key(id));
        self.detailed.retain(|id| records.contains_key(id));
        self.pending.retain(|id, _| records.contains_key(id));
        self.runtime_titles.retain(|id, _| records.contains_key(id));
        for (id, title) in &self.runtime_titles {
            if let Some(record) = self.controller.state.sessions.get_mut(id) {
                record.acp_session_title = title.clone();
            }
        }
        for id in self.lifecycles.difference(&active) {
            self.dashboard.finish_session_operation(id);
        }
        // Remove old overlays before applying durable records; otherwise a
        // completed resume would turn Running back into Provisioning here.
        self.dashboard.set_state(self.controller.state.clone());
        for lifecycle in &snapshot.lifecycles {
            if self
                .controller
                .state
                .sessions
                .contains_key(&lifecycle.session_id)
            {
                apply_lifecycle_display(&mut self.dashboard, lifecycle);
            }
        }
        self.lifecycles = active;
        self.dashboard.set_session_reviews(snapshot.reviews);
        for notice in snapshot.notices {
            if self
                .reported_notice_id
                .is_none_or(|previous| notice.id > previous)
            {
                self.reported_notice_id = Some(notice.id);
                self.runtime_notices.set(notice.text);
            }
        }
        let lifecycle_notices: BTreeMap<_, _> = snapshot
            .lifecycles
            .into_iter()
            .filter_map(|op| op.notice.map(|notice| (op.session_id, notice)))
            .collect();
        for (id, notice) in &lifecycle_notices {
            if self.lifecycle_notices.get(id) != Some(notice) {
                self.runtime_notices.set(notice.clone());
            }
        }
        self.lifecycle_notices = lifecycle_notices;
    }

    fn apply_session(&mut self, session_id: String, view: ManagedSessionView) {
        let Some(record) = self.controller.state.sessions.get_mut(&session_id) else {
            return;
        };
        apply_session_activity(&mut self.dashboard, &session_id, &view);
        let key = (session_id.clone(), "runtime");
        match &view.error {
            Some(error) => {
                self.errors
                    .insert(key, format!("Session {session_id}: {error:?}"));
            }
            None => {
                self.errors.remove(&key);
            }
        }
        if let Some(snapshot) = view.snapshot {
            // Resolve window-dependent provisional titles exactly as the
            // dashboard does, but only in this in-memory preview.
            let title = snapshot.resolved_title();
            let changed = record.acp_session_title != title;
            record.acp_session_title = title;
            self.runtime_titles
                .insert(session_id.clone(), record.acp_session_title.clone());
            if changed {
                self.dashboard.set_state(self.controller.state.clone());
            }
            self.pending
                .insert(session_id.clone(), snapshot.materialized);
            self.start_projection(&session_id);
        }
    }

    fn prepare_records(&mut self) {
        for session in self.controller.state.sessions.values() {
            let session_id = session.id.clone();
            if self.summaries_started.insert(session_id.clone()) {
                let permits = self.permits.clone();
                let viewed = session.viewed_through_event_ordinal;
                let id = session_id.clone();
                self.tasks.spawn(async move {
                    let result = prepare_blocking(permits, move || {
                        Ok(
                            hel::hel_database::load_materialized_session_summary(&id)?.map(
                                |summary| {
                                    let title = summary
                                        .session_title
                                        .as_deref()
                                        .and_then(hel::hel_state::normalize_session_title);
                                    (
                                        PreparedMaterializedSessionSummary::from_materialized(
                                            summary, viewed,
                                        ),
                                        title,
                                    )
                                },
                            ),
                        )
                    })
                    .await;
                    PreparedUpdate::Summary { session_id, result }
                });
            }
            let session_id = session.id.clone();
            if self.sources_started.insert(session_id.clone()) {
                let controller = Controller {
                    config: self.controller.config.clone(),
                    state: HelState {
                        sessions: [(session_id.clone(), session.clone())]
                            .into_iter()
                            .collect(),
                        ..HelState::default()
                    },
                };
                let permits = Arc::clone(&SOURCE_PERMITS);
                let cancelled = self.cancelled.clone();
                let generation = self.source_generation;
                let id = session_id.clone();
                self.tasks.spawn(async move {
                    let result = prepare_blocking(permits, move || {
                        let executor = CancellableProcessExecutor::new(cancelled)
                            .with_deadline(Duration::from_secs(8));
                        controller.resolve_session_project_source(&id, &executor)
                    })
                    .await;
                    PreparedUpdate::Source {
                        session_id,
                        generation,
                        result,
                    }
                });
            }
        }
    }

    fn start_projection(&mut self, session_id: &str) {
        if self.projecting.contains(session_id) {
            return;
        }
        let Some(materialized) = self.pending.remove(session_id) else {
            return;
        };
        let Some(session) = self.controller.state.sessions.get(session_id) else {
            return;
        };
        let viewed = session.viewed_through_event_ordinal;
        let previous = self.dashboard.take_projection_cache(session_id);
        let session_id = session_id.to_owned();
        self.projecting.insert(session_id.clone());
        let permits = self.permits.clone();
        self.tasks.spawn(async move {
            let result = prepare_blocking(permits, move || {
                Ok(PreparedMaterializedSessionDetail::from_materialized(
                    materialized,
                    viewed,
                    previous,
                ))
            })
            .await;
            PreparedUpdate::Detail {
                session_id,
                result: result.map(Box::new),
            }
        });
    }

    fn apply_prepared(&mut self, update: PreparedUpdate) {
        match update {
            PreparedUpdate::Metadata(result) => {
                self.metadata_in_flight = false;
                self.metadata_refresh_at = Instant::now() + Duration::from_secs(5);
                if let Some(metadata) = self.record_result(String::new(), "metadata", result) {
                    self.metadata = Some(metadata);
                }
            }
            PreparedUpdate::Summary { session_id, result } => {
                if !self.controller.state.sessions.contains_key(&session_id)
                    || self.detailed.contains(&session_id)
                {
                    return;
                }
                if let Some(Some((summary, title))) =
                    self.record_result(session_id.clone(), "summary", result)
                    && self
                        .dashboard
                        .apply_prepared_materialized_session_summary(summary)
                    && let Some(title) = title
                {
                    self.controller
                        .state
                        .sessions
                        .get_mut(&session_id)
                        .expect("checked membership")
                        .acp_session_title = Some(title.clone());
                    self.runtime_titles.insert(session_id, Some(title));
                }
            }
            PreparedUpdate::Source {
                session_id,
                generation,
                result,
            } => {
                if generation != self.source_generation
                    || !self.controller.state.sessions.contains_key(&session_id)
                {
                    return;
                }
                if let Some(source) = self.record_result(session_id.clone(), "project", result) {
                    self.dashboard.set_project_source(&session_id, source);
                }
            }
            PreparedUpdate::Detail { session_id, result } => {
                self.projecting.remove(&session_id);
                if !self.controller.state.sessions.contains_key(&session_id) {
                    return;
                }
                if let Some(detail) = self.record_result(session_id.clone(), "detail", result)
                    && self.dashboard.apply_prepared_materialized_session(*detail)
                {
                    self.detailed.insert(session_id.clone());
                    self.errors.remove(&(session_id.clone(), "summary"));
                }
                self.start_projection(&session_id);
            }
        }
    }

    fn record_result<T>(
        &mut self,
        session_id: String,
        kind: &'static str,
        result: Result<T>,
    ) -> Option<T> {
        let key = (session_id, kind);
        match result {
            Ok(value) => {
                self.errors.remove(&key);
                Some(value)
            }
            Err(error) => {
                self.errors
                    .insert(key, format!("Could not load {kind}: {error:#}"));
                None
            }
        }
    }
}

async fn prepare_blocking<T: Send + 'static>(
    permits: Arc<tokio::sync::Semaphore>,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let permit = permits
        .acquire_owned()
        .await
        .context("preview workers stopped")?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let result = work();
        if let Err(error) = &result {
            tracing::warn!(%error, "workspace preview preparation failed");
        }
        result
    })
    .await
    .context("preview preparation task failed")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use hel::hel_config::HarnessKind;
    use hel::hel_state::{
        MaterializedExecutionState, MaterializedSessionSummary, SessionRecord, SessionState,
    };
    use hel::hel_workspace::WorkspaceRecord;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn session(id: &str) -> SessionRecord {
        SessionRecord {
            id: id.into(),
            workspace_id: "workspace".into(),
            title: id.into(),
            harness_kind: HarnessKind::Codex,
            last_profile: "codex".into(),
            bundle_id: "project".into(),
            project_directory: None,
            managed_worktree: None,
            target_template_id: "local".into(),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            container_cpus: None,
            container_memory: None,
            state: SessionState::Running,
            archived: false,
            target: None,
            native_session_id: None,
            acp_session_title: None,
            session_title_override: None,
            created_at: "2026-09-01T00:00:00Z".into(),
            updated_at: "2026-09-01T00:00:00Z".into(),
            viewed_through_event_ordinal: 7,
            draft_input: "unsent draft".into(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        }
    }

    fn snapshot(records: Vec<SessionRecord>) -> daemon::RuntimeSnapshot {
        daemon::RuntimeSnapshot {
            revision: 1,
            config: HelConfig::default(),
            records,
            sessions: Vec::new(),
            lifecycles: Vec::new(),
            reviews: Vec::new(),
            notices: Vec::new(),
        }
    }

    fn text(preview: &WorkspacePreview) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 25)).unwrap();
        let mut scroll = hel_tui::SessionsPreviewState::default();
        terminal
            .draw(|frame| {
                hel_tui::render_sessions_preview(
                    frame,
                    frame.area(),
                    &preview.dashboard,
                    &mut scroll,
                )
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn detail(id: &str, title: &str, ordinal: u64) -> PreparedUpdate {
        let mut materialized = MaterializedSession::empty(id);
        materialized.session_title = Some(title.into());
        materialized.applied_event_ordinal = ordinal;
        PreparedUpdate::Detail {
            session_id: id.into(),
            result: Ok(Box::new(
                PreparedMaterializedSessionDetail::from_materialized(
                    materialized,
                    7,
                    Default::default(),
                ),
            )),
        }
    }

    #[test]
    fn preview_loads_live_rows_retains_them_on_failure_and_accepts_removals() {
        let mut preview = WorkspacePreview::empty("workspace".into(), "Workspace".into());
        assert_eq!(preview.status().as_deref(), Some("Loading sessions…"));
        assert!(!preview.metadata_ready());
        let mut stopped = session("stopped-session");
        stopped.state = SessionState::Stopped;
        preview.apply_snapshot(snapshot(vec![session("first-session"), stopped]));
        preview.apply_prepared(detail("first-session", "Live session title", 9));
        assert!(preview.loaded);
        assert_eq!(preview.session_count(), Some(1));
        let drawn = text(&preview);
        assert!(drawn.contains("Live session title"), "{drawn}");
        assert!(!drawn.contains("stopped-session"), "{drawn}");

        preview.apply_runtime(RuntimeFeedUpdate::Error("Connection lost".into()));
        assert!(preview.status().unwrap().contains("stale"));
        assert!(text(&preview).contains("Live session title"));
        preview.apply_snapshot(snapshot(Vec::new()));
        assert!(preview.status().is_none());
        assert!(text(&preview).contains("No active sessions"));
        preview.apply_prepared(detail("first-session", "Late removed session", 10));
        assert!(!text(&preview).contains("Late removed session"));
    }

    fn stored_summary() -> PreparedUpdate {
        let summary = PreparedMaterializedSessionSummary::from_materialized(
            MaterializedSessionSummary {
                session_id: "session".into(),
                applied_event_ordinal: 9,
                last_activity_at_ms: None,
                execution: MaterializedExecutionState::Idle,
                session_title: Some("Old summary title".into()),
                last_agent_message: Some("Old summary message".into()),
                last_user_message: None,
                last_agent_message_follows_last_user: true,
                agent_message_latest_content_ordinals: vec![9],
                session_restart_event_ordinals: Vec::new(),
            },
            7,
        );
        PreparedUpdate::Summary {
            session_id: "session".into(),
            result: Ok(Some((summary, Some("Old summary title".into())))),
        }
    }

    #[test]
    fn a_stored_title_survives_record_refresh_without_a_live_relay() {
        let mut preview = WorkspacePreview::empty("workspace".into(), "Workspace".into());
        preview.apply_snapshot(snapshot(vec![session("session")]));
        preview.apply_prepared(stored_summary());
        assert!(text(&preview).contains("Old summary title"));
        preview.apply_snapshot(snapshot(vec![session("session")]));
        assert!(text(&preview).contains("Old summary title"));
    }

    #[test]
    fn a_late_startup_summary_cannot_replace_live_detail_or_mark_it_read() {
        let mut preview = WorkspacePreview::empty("workspace".into(), "Workspace".into());
        preview.apply_snapshot(snapshot(vec![session("session")]));
        preview.apply_prepared(detail("session", "New live title", 9));
        preview.apply_prepared(stored_summary());
        let drawn = text(&preview);
        assert!(drawn.contains("New live title"), "{drawn}");
        assert!(!drawn.contains("Old summary"), "{drawn}");
        let record = &preview.controller.state.sessions["session"];
        assert_eq!(record.viewed_through_event_ordinal, 7);
        assert_eq!(record.draft_input, "unsent draft");
    }

    #[test]
    fn stale_project_resolution_cannot_replace_current_workspace_grouping() {
        let mut preview = WorkspacePreview::empty("workspace".into(), "Workspace".into());
        preview.apply_snapshot(snapshot(vec![session("session")]));
        let generation = preview.source_generation;
        let mut moved = session("session");
        moved.project_directory = Some(std::path::PathBuf::from("/different/checkout"));
        preview.apply_snapshot(snapshot(vec![moved]));
        preview.apply_prepared(PreparedUpdate::Source {
            session_id: "session".into(),
            generation,
            result: Err(anyhow::anyhow!("old configuration failed")),
        });
        assert!(preview.status().is_none());
        assert!(!text(&preview).contains("old configuration failed"));
    }

    #[test]
    fn stopped_sessions_become_visible_while_resuming_and_leave_when_settled() {
        let mut preview = WorkspacePreview::empty("workspace".into(), "Workspace".into());
        let mut stopped = session("resuming-session");
        stopped.state = SessionState::Stopped;
        let mut resuming = snapshot(vec![stopped.clone()]);
        resuming.lifecycles.push(daemon::RuntimeLifecycleView {
            session_id: stopped.id.clone(),
            kind: daemon::RuntimeLifecycleKind::Resume,
            started_at_epoch_seconds: 1,
            active_stages: Vec::new(),
            resume_destination: Some(("new-profile".into(), "new-target".into())),
            notice: None,
        });
        preview.apply_snapshot(resuming);
        let drawn = text(&preview);
        assert!(drawn.contains("Resuming"), "{drawn}");
        assert!(drawn.contains("new-profile"), "{drawn}");
        assert_eq!(preview.session_count(), Some(1));
        preview.apply_snapshot(snapshot(vec![session("resuming-session")]));
        let settled = text(&preview);
        assert!(settled.contains("resuming-session"), "{settled}");
        assert!(
            !settled.contains("Resuming") && !settled.contains("Launch"),
            "{settled}"
        );
        preview.apply_snapshot(snapshot(vec![stopped]));
        assert!(text(&preview).contains("No active sessions"));
        assert_eq!(preview.session_count(), Some(0));
    }

    #[test]
    fn runtime_notices_are_visible_once_while_the_preview_is_open() {
        let mut preview = WorkspacePreview::empty("workspace".into(), "Workspace".into());
        let mut update = snapshot(Vec::new());
        update.notices.push(daemon::RuntimeNotice {
            id: 3,
            session_id: "session".into(),
            text: "Recovery needs attention".into(),
        });
        preview.apply_snapshot(update.clone());
        assert_eq!(
            preview.status().as_deref(),
            Some("Recovery needs attention")
        );
        preview.runtime_notices.clear();
        preview.apply_snapshot(update);
        assert!(preview.status().is_none());
    }

    #[test]
    fn failed_metadata_refresh_disables_management_without_discarding_the_preview() {
        let mut preview = WorkspacePreview::empty("workspace".into(), "Workspace".into());
        let metadata = || WorkspaceSnapshot {
            workspace: WorkspaceRecord {
                id: "workspace".into(),
                name: "Workspace".into(),
                created_at: "2026-09-01T00:00:00Z".into(),
                last_opened_at: "2026-09-01T00:00:00Z".into(),
                session_count: 0,
            },
            sessions: Vec::new(),
            drafts: Vec::new(),
        };
        preview.apply_prepared(PreparedUpdate::Metadata(Ok(metadata())));
        assert!(preview.metadata_ready());
        preview.apply_prepared(PreparedUpdate::Metadata(Err(anyhow::anyhow!(
            "metadata unavailable"
        ))));
        assert!(!preview.metadata_ready());
        assert!(preview.metadata.is_some());
        assert!(preview.status().unwrap().contains("metadata unavailable"));
        preview.apply_prepared(PreparedUpdate::Metadata(Ok(metadata())));
        assert!(preview.metadata_ready());
    }

    #[tokio::test]
    async fn switching_workspace_cancels_delayed_preparation_and_discards_old_completions() {
        struct NotifyDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for NotifyDrop {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        let mut old = WorkspacePreview::empty("old".into(), "Old".into());
        let cancelled = old.cancelled.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (stopped_tx, stopped_rx) = tokio::sync::oneshot::channel();
        old.tasks.spawn(async move {
            let _guard = NotifyDrop(Some(stopped_tx));
            started_tx.send(()).unwrap();
            std::future::pending::<()>().await;
            detail("old-session", "Should never arrive", 10)
        });
        started_rx.await.unwrap();
        // A queued completion also belongs solely to the old subscription.
        old.tasks
            .spawn(async { detail("old-session", "Queued old detail", 11) });
        let current = WorkspacePreview::empty("current".into(), "Current".into());
        drop(old);
        tokio::time::timeout(Duration::from_secs(1), stopped_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(cancelled.load(Ordering::Acquire));
        assert!(!current.loaded);
        assert!(!text(&current).contains("old"));
    }
}

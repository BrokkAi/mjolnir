//! One-shot background jobs the dashboard starts and the updates they report.
//!
//! Each job runs on a blocking task and answers over the dashboard's single
//! [`DashboardIoUpdate`] channel, so no filesystem, database, or process work
//! ever happens on the render loop. Failures travel as `Err` payloads rather
//! than being dropped.

mod spawn;
pub(crate) use spawn::*;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mj_controller::database::DetachedSessionDraft;
use mj_core::config::{Config, ProjectBundle, is_bare_project_target};
use mj_core::remote_git::{default_branch, display_url, resolve_repository};
use mj_core::state::{
    MaterializedSession, MovePreparation, ProjectSourceIdentity, SessionRecord, SessionState, State,
};

use mj_controller::controller::Controller;
use mj_controller::controller::ResumeRepositorySourcePreflight;
use mj_controller::session_manager::SessionManagerControl;
use mj_controller::targets::CancellableProcessExecutor;
use mj_tui::{
    DashboardAction, PreparedMaterializedSessionDetail, PreparedMaterializedSessionSummary,
    RemoteRepositoryPreview, ReviewSettingsChoices, ReviewSettingsDiscoveryResult,
    SessionOperationKind, WebViewerAccess,
};
use mj_tui::{WorkspaceDraftEntry, WorkspaceManagementEntry};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::daemon;
use crate::dashboard::{CriticalOperationTracker, DashboardContext};
use crate::import::{DashboardImportSuccess, PendingDashboardImport, persist_imported_session};
use crate::pollers::{
    LifecycleSuccess, LifecycleUpdate, WorkerRecordPersistence, WorkerRecordPersistenceOutcome,
};
use crate::short_id;

/// Everything the dashboard learns from a background job.
pub(crate) enum DashboardIoUpdate {
    RepositoryRemotesRepaired {
        generation: Option<u64>,
        retry: Box<DashboardAction>,
        result: std::result::Result<(), String>,
    },
    /// A workspace manager snapshot loaded away from the event loop. The
    /// selection hint is set only by a successful create; the dashboard must
    /// still verify the modal generation before applying it.
    WorkspaceManagement {
        generation: u64,
        result: std::result::Result<WorkspaceManagementResult, String>,
    },
    WorkspacePaneSizes {
        result: std::result::Result<BTreeMap<String, mj_core::workspace::PaneSizes>, String>,
    },
    WorkerRecordPersistence {
        operation: WorkerRecordPersistence,
        result: std::result::Result<WorkerRecordPersistenceOutcome, String>,
    },
    MaterializedSessionProjection {
        session_id: String,
        result: std::result::Result<Box<PreparedMaterializedSessionDetail>, String>,
    },
    /// A bounded tail of a resumed session's stored transcript, loaded off the
    /// event loop so the conversation is not blank while the poller catches up.
    TranscriptTailSeed {
        materialized: Box<MaterializedSession>,
    },
    StoredSessionSummary {
        session_id: String,
        result: std::result::Result<PreparedMaterializedSessionSummary, String>,
    },
    ProjectSource {
        session_id: String,
        result: std::result::Result<ProjectSourceIdentity, String>,
    },
    ChatOpened {
        generation: u64,
        session_id: String,
        result: Box<std::result::Result<mj_chat::chat::PreparedChat, String>>,
    },
    /// The daemon refused a review action. The message is a sentence for the
    /// person who pressed the key, so it goes back to the chat that sent it.
    ReviewRefused {
        session_id: String,
        message: String,
    },
    CreateSession(Box<DashboardCreateSessionUpdate>),
    GoSelectionSaved(std::result::Result<(), String>),
    GoContext {
        session_id: String,
        result: std::result::Result<(std::path::PathBuf, String), String>,
    },
    GoPrepared {
        workspace_id: String,
        retry: Box<DashboardAction>,
        result: Box<std::result::Result<(Config, mj_core::go::GoRecipe), String>>,
    },
    RemotePreflight {
        generation: u64,
        launch: Box<DashboardAction>,
        result: std::result::Result<RemotePreflightOutcome, String>,
    },
    RenameSession {
        session_id: String,
        title: String,
        result: std::result::Result<String, String>,
    },
    ContainerSettings {
        session_id: String,
        result: std::result::Result<Controller, String>,
    },
    TargetReadiness {
        generation: u64,
        target_id: String,
        result: std::result::Result<(), String>,
    },
    TargetTest {
        target_id: String,
        result: std::result::Result<(), String>,
    },
    ConfigRename {
        what: String,
        result: std::result::Result<Controller, String>,
    },
    ConfigReloaded(std::result::Result<Controller, String>),
    WebAccess {
        generation: u64,
        access: WebViewerAccess,
    },
    WebListeners {
        generation: u64,
        result: Result<Vec<mj_tui::WebListenerProcess>, String>,
    },
    WebAccessError {
        generation: u64,
        error: String,
    },
    SetupSaved {
        generation: u64,
        result: std::result::Result<Config, String>,
    },
    SetupDiscovered {
        generation: u64,
        result: std::result::Result<Config, String>,
    },
    ReviewSettingsDiscovered {
        generation: u64,
        profile_id: String,
        model: Option<String>,
        result: std::result::Result<ReviewSettingsDiscoveryResult, String>,
    },
    ReviewSettingsChoices {
        generation: u64,
        profile_id: String,
        model: Option<String>,
        choices: mj_controller::review_settings::ReviewCapabilityChoices,
    },
    SpinnerStyleSaved {
        result: std::result::Result<Config, String>,
    },
    DetachedSessionState {
        session_id: String,
        result: std::result::Result<(), String>,
    },
    ReadReceipt {
        session_id: String,
        result: std::result::Result<u64, String>,
    },
    CreatedBundle {
        result: Box<std::result::Result<CreatedBundleUpdate, String>>,
    },
    ImportedSessionApplied {
        result: Box<std::result::Result<ImportedDashboardSessionApply, String>>,
    },
    LifecycleReloaded(Box<LifecycleReloaded>),
    LifecycleCancellation {
        session_id: String,
        result: std::result::Result<(), String>,
    },
    MovePrepared {
        session_id: String,
        request_id: u64,
        result: std::result::Result<MovePreparation, String>,
    },
    CheckpointArchiveSizes {
        generation: u64,
        sizes: BTreeMap<String, Option<u64>>,
    },
    WorkerDiagnosis {
        session_id: String,
        episode_id: u64,
        result: std::result::Result<Option<String>, String>,
    },
    MountCompletions {
        prefix: String,
        result: std::result::Result<Vec<String>, String>,
    },
    MountValidation {
        context: String,
        source: String,
        result: std::result::Result<(std::path::PathBuf, Option<String>), String>,
    },
    SessionMountValidation {
        generation: u64,
        launch: Box<DashboardAction>,
        result: std::result::Result<Option<(String, String)>, String>,
    },
    ResumeRepositoryPreflight {
        generation: u64,
        launch: Box<DashboardAction>,
        submitted_repository_id: Option<String>,
        result: Box<std::result::Result<ResumeRepositoryPreflightApply, String>>,
    },
    ContainerPathResolved {
        context: String,
        result: std::result::Result<std::path::PathBuf, String>,
    },
    SetupPathResolved {
        generation: u64,
        draft: serde_json::Value,
        path: Vec<String>,
        value: String,
        result: std::result::Result<std::path::PathBuf, String>,
    },
    ProjectValidation {
        context: String,
        directory: String,
        result: std::result::Result<
            (std::path::PathBuf, mj_core::state::ManagedWorktreeOptions),
            String,
        >,
    },
    /// Clipboard providers may use a blocking desktop IPC call. The result
    /// is delivered here after that work finishes on a blocking task.
    ClipboardText(std::result::Result<String, String>),
    /// A copied selection reached the desktop clipboard, or did not. Only a
    /// failure needs reporting: the notice for a successful copy is already
    /// on screen.
    ClipboardWritten(std::result::Result<(), String>),
}

pub(crate) struct ActiveLifecycleOperation {
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) kind: SessionOperationKind,
    pub(crate) retry_launch: Option<DashboardAction>,
}

pub(crate) struct WorkspaceManagementResult {
    revision: u64,
    pub(crate) entries: Vec<WorkspaceManagementEntry>,
    pub(crate) select_workspace: Option<String>,
    pub(crate) deleted_workspace_id: Option<String>,
}

pub(crate) struct RegisteredDashboardSession {
    retry_launch: DashboardAction,
    session: SessionRecord,
    remembered_container_size: Option<(String, mj_core::state::HostContainerSize)>,
    cancelled: Arc<AtomicBool>,
}

pub(crate) enum DashboardCreateSessionUpdate {
    RemoteRepair {
        bundle_id: String,
        repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
        retry: Box<DashboardAction>,
    },
    Registered(Box<RegisteredDashboardSession>),
    Failed {
        error: String,
        retry_launch: Box<DashboardAction>,
    },
}

pub(crate) enum RemotePreflightOutcome {
    Ready(Vec<RemoteRepositoryPreview>),
    Repair(Vec<mj_core::local_git::LocalRemoteRepair>),
}

pub(crate) struct ImportedDashboardSessionApply {
    harness: &'static str,
    native_session_id: String,
    session: SessionRecord,
    bundle_id: String,
    bundle: ProjectBundle,
}

pub(crate) struct CreatedBundleUpdate {
    config: Config,
    bundle_id: String,
}

pub(crate) struct ResumeRepositoryPreflightApply {
    pub(crate) config: Option<Config>,
    pub(crate) preflight: ResumeRepositorySourcePreflight,
}

pub(crate) struct LifecycleReload {
    pub(crate) update: LifecycleUpdate,
    pub(crate) operation: Option<ActiveLifecycleOperation>,
}

pub(crate) struct LifecycleReloaded {
    reload: LifecycleReload,
    result: std::result::Result<Controller, String>,
}

fn resolve_remote_repositories(
    config: &Config,
    bundle_id: &str,
    target_template_id: &str,
    executor: &impl mj_controller::targets::CommandExecutor,
) -> Result<Vec<RemoteRepositoryPreview>> {
    let target = config
        .targets
        .get(target_template_id)
        .with_context(|| format!("unknown target template {target_template_id:?}"))?;
    if is_bare_project_target(target) {
        return Ok(Vec::new());
    }
    let bundle = config
        .bundles
        .get(bundle_id)
        .with_context(|| format!("unknown bundle {bundle_id:?}"))?;
    bundle
        .repositories
        .iter()
        .map(|repository| {
            let source = resolve_repository(repository, executor)
                .with_context(|| format!("repository {:?}", repository.id))?;
            let default_branch = default_branch(&source, executor)
                .with_context(|| format!("repository {:?}", repository.id))?;
            Ok(RemoteRepositoryPreview {
                repository_id: repository.id.clone(),
                fetch_url: display_url(&source.fetch_url),
                default_branch,
                push_urls: source
                    .push_urls
                    .iter()
                    .map(|url| display_url(url))
                    .collect(),
            })
        })
        .collect()
}

impl DashboardContext {
    /// Folds one finished background job into dashboard and controller state.
    pub(super) fn apply_dashboard_io_update(&mut self, update: DashboardIoUpdate) {
        match update {
            DashboardIoUpdate::WorkspacePaneSizes { result } => match result {
                Ok(layouts) => {
                    for (workspace_id, sizes) in layouts {
                        if !self.known_workspace_layouts.contains(&workspace_id) {
                            continue;
                        }
                        self.dashboard
                            .cache_workspace_pane_sizes(&workspace_id, sizes);
                        self.pane_size_persistence.remember(workspace_id, sizes);
                    }
                }
                Err(error) => self
                    .dashboard
                    .set_notice(format!("Could not load workspace pane sizes: {error}")),
            },
            DashboardIoUpdate::WorkspaceManagement { generation, result } => match result {
                Ok(result) => {
                    let WorkspaceManagementResult {
                        revision,
                        entries,
                        select_workspace,
                        deleted_workspace_id,
                    } = result;
                    // A management response may precede an already queued
                    // runtime feed message. Do not let that older message
                    // remove a newly created tab or resurrect a deleted one.
                    let names_are_current = revision >= self.runtime_state_revision;
                    self.runtime_state_revision = self.runtime_state_revision.max(revision);
                    let names: BTreeMap<String, String> = entries
                        .iter()
                        .map(|entry| (entry.workspace.id.clone(), entry.workspace.name.clone()))
                        .collect();
                    if let Some(deleted_workspace_id) = deleted_workspace_id.as_ref() {
                        self.pane_size_persistence.forget(deleted_workspace_id);
                        self.known_workspace_layouts.remove(deleted_workspace_id);
                    }
                    // The TUI owns the modal generation guard. It returns
                    // whether this result still belongs to the visible
                    // manager, so a late create cannot switch another tab.
                    let foreground = self
                        .dashboard
                        .finish_workspace_management(generation, Ok(entries));
                    let next_workspace = deleted_workspace_id
                        .as_ref()
                        .and_then(|deleted_id| names.keys().find(|id| *id != deleted_id).cloned());
                    if names_are_current {
                        self.dashboard.set_workspace_names(names);
                    }
                    if foreground
                        && let Some(workspace_id) = select_workspace
                        && (names_are_current
                            || self.known_workspace_layouts.contains(&workspace_id))
                    {
                        self.dashboard.cancel_modal();
                        self.select_workspace(Some(workspace_id));
                    } else if let Some(deleted_workspace_id) = deleted_workspace_id
                        && self.dashboard.active_workspace_id()
                            == Some(deleted_workspace_id.as_str())
                    {
                        self.select_workspace(next_workspace);
                    }
                }
                Err(error) => {
                    self.dashboard
                        .finish_workspace_management(generation, Err(error));
                }
            },
            DashboardIoUpdate::ReviewRefused {
                session_id,
                message,
            } => {
                match self
                    .active_chat
                    .as_mut()
                    .filter(|chat| chat.session_id() == session_id)
                {
                    Some(chat) => chat.report_review_refusal(message),
                    // The chat moved on; the refusal still belongs on screen.
                    None => self.dashboard.set_notice(message),
                }
            }
            DashboardIoUpdate::WorkerRecordPersistence { operation, result } => {
                match (operation, result) {
                    (WorkerRecordPersistence::AcpTitle { .. }, Err(error)) => self
                        .dashboard
                        .set_notice(format!("Could not save harness title: {error}")),
                    (
                        WorkerRecordPersistence::TargetMissing {
                            session_id,
                            detail,
                            updated_at,
                        },
                        Ok(WorkerRecordPersistenceOutcome::TargetMissing(state)),
                    ) => {
                        if let Some(session) = self.controller.state.sessions.get_mut(&session_id)
                            && matches!(
                                session.state,
                                SessionState::Provisioning
                                    | SessionState::Running
                                    | SessionState::Disconnected
                                    | SessionState::Error
                            )
                        {
                            session.state = state;
                            session.last_error = Some(detail);
                            session.updated_at = updated_at;
                            self.dashboard.set_state(self.controller.state.clone());
                            self.drop_warm_chat_for(&session_id);
                            self.refresh_poll_targets();
                            let notice = match state {
                                SessionState::Error => format!(
                                    "Session {} cannot reach its managed target; its last verified checkpoint is ready to resume",
                                    short_id(&session_id)
                                ),
                                SessionState::Lost => format!(
                                    "Session {} is lost because its managed target no longer exists",
                                    short_id(&session_id)
                                ),
                                _ => unreachable!("a missing target persisted as {state:?}"),
                            };
                            self.dashboard.set_notice(notice);
                        }
                    }
                    (WorkerRecordPersistence::TargetMissing { session_id, .. }, Err(error)) => {
                        self.dashboard.set_notice(format!(
                            "Could not record missing target for {}: {error}",
                            short_id(&session_id)
                        ))
                    }
                    (
                        WorkerRecordPersistence::AcpTitle { .. },
                        Ok(WorkerRecordPersistenceOutcome::Saved),
                    )
                    | (
                        WorkerRecordPersistence::TargetMissing { .. },
                        Ok(WorkerRecordPersistenceOutcome::Unchanged),
                    ) => {}
                    (operation, Ok(outcome)) => {
                        unreachable!("persistence operation {operation:?} returned {outcome:?}")
                    }
                }
            }
            DashboardIoUpdate::MaterializedSessionProjection { session_id, result } => {
                self.finish_materialized_projection(session_id, result);
            }
            DashboardIoUpdate::TranscriptTailSeed { materialized } => {
                let viewed_through_event_ordinal = self
                    .controller
                    .state
                    .sessions
                    .get(&materialized.session_id)
                    .map_or(0, |session| session.viewed_through_event_ordinal);
                self.request_materialized_projection(*materialized, viewed_through_event_ordinal);
            }
            DashboardIoUpdate::StoredSessionSummary { session_id, result } => {
                match result {
                    Ok(summary) => {
                        self.dashboard
                            .apply_prepared_materialized_session_summary(summary);
                    }
                    Err(error) => tracing::warn!(
                        %session_id,
                        "could not restore stored session summary: {error}"
                    ),
                }
                // Either way this session has answered, so the startup pick is
                // one summary closer to being able to choose.
                self.finish_startup_summary(&session_id);
            }
            DashboardIoUpdate::ProjectSource { session_id, result } => {
                self.project_sources_in_flight.remove(&session_id);
                match result {
                    Ok(source) => self.dashboard.set_project_source(&session_id, source),
                    Err(error) => tracing::warn!(
                        %session_id,
                        "could not resolve canonical project source: {error}"
                    ),
                }
            }
            DashboardIoUpdate::ChatOpened {
                generation,
                session_id,
                result,
            } => {
                // Ignore a late result after a newer request has taken its
                // place (or the dashboard has shut down).
                if !self
                    .attachment
                    .accepts(generation, self.dashboard.selected_session_id())
                    || self.opening_chat_session.as_deref() != Some(session_id.as_str())
                {
                    return;
                }
                self.opening_chat_session = None;
                self.dashboard.set_opening_session(None);
                // The attach may have crossed a lifecycle boundary while it
                // was preparing. Keep the warm chat/draft untouched and drop
                // the late result instead of reviving a retiring conversation.
                if self.dashboard.transition_kind(&session_id).is_some()
                    || self
                        .dashboard
                        .transition_failure_kind(&session_id)
                        .is_some()
                {
                    self.dashboard.set_current_session(None);
                    self.defer_chat_open();
                    self.dirty = true;
                    return;
                }
                match *result {
                    Ok(chat) => {
                        // The old warm chat continued receiving feed updates
                        // while this attach was in flight. Capture and persist
                        // its latest local composer just before replacing it.
                        if let Some(ordinal) = self
                            .active_chat
                            .as_ref()
                            .map(mj_chat::chat::ActiveChat::latest_event_ordinal)
                        {
                            self.record_detach(ordinal);
                        }
                        self.save_active_question_draft();
                        // Whatever the user typed into the standby composer
                        // while this attach ran is the newest draft, so it
                        // wins over the copy captured when the open started.
                        let chat = if let Some(text) =
                            self.dashboard.take_standby_prompt_draft(&session_id)
                        {
                            chat.with_draft(text)
                        } else if let Some(draft) = self.composer_drafts.get(&session_id) {
                            chat.with_draft(draft.text.clone())
                        } else {
                            chat
                        };
                        let mut chat = chat.open_replacing(self.active_chat.as_ref());
                        self.restore_question_draft(&session_id, &mut chat);
                        self.active_chat = Some(chat);
                        // The context travelled with the attach, which is
                        // asynchronous; anything the surface learned while it
                        // was in flight is handed over now.
                        self.refresh_chat_context();
                        self.apply_runtime_review_to_active_chat();
                        self.dashboard.set_current_session(Some(&session_id));
                        self.dashboard.clear_notice();
                        self.acknowledge_visible_chat();
                    }
                    Err(error) => {
                        tracing::warn!(%session_id, %error, "could not open session");
                        self.dashboard.set_notice(format!("Could not open session: {error}. Press Enter in Sessions to retry, or select another session. Alt-Q quits."));
                    }
                }
                self.dirty = true;
            }
            DashboardIoUpdate::GoSelectionSaved(result) => {
                self.go_selection_in_flight = false;
                if let Err(error) = result {
                    self.dashboard.set_failure_notice(format!(
                        "Could not remember this conversation: {error}"
                    ));
                }
            }
            DashboardIoUpdate::GoContext { session_id, result } => {
                self.go_context_in_flight = false;
                self.dashboard.set_go_context(session_id, result);
            }
            DashboardIoUpdate::GoPrepared {
                workspace_id,
                retry,
                result,
            } => match *result {
                Ok((config, recipe)) => {
                    self.controller.config = config.clone();
                    let action = self
                        .dashboard
                        .go_launch_action(workspace_id, config, recipe);
                    super::actions::start_session_launch(self, action);
                }
                Err(error) => {
                    if self.dashboard.active_workspace_id() == Some(workspace_id.as_str()) {
                        self.dashboard.show_launch_failure(error, Some(*retry));
                    } else {
                        self.dashboard.set_failure_notice(format!("Session preparation failed in workspace {workspace_id}: {error}. Return to that workspace to retry."));
                    }
                }
            },
            DashboardIoUpdate::CreateSession(update) => self.apply_create_session_update(*update),
            DashboardIoUpdate::RemotePreflight {
                generation,
                launch,
                result,
            } => {
                if generation == self.dashboard.session_preflight_generation() {
                    match result {
                        Ok(RemotePreflightOutcome::Repair(repairs)) => {
                            self.dashboard.apply_remote_session_preflight(
                                generation,
                                Err("Git tracking needs repair".into()),
                            );
                            if let DashboardAction::CreateSession { bundle_id, .. } = &*launch {
                                self.dashboard.show_remote_repair_confirmation(
                                    bundle_id.clone(),
                                    repairs,
                                    DashboardAction::PreflightCreateSession { launch },
                                );
                            }
                        }
                        other => self.dashboard.apply_remote_session_preflight(
                            generation,
                            other.map(|outcome| match outcome {
                                RemotePreflightOutcome::Ready(repositories) => repositories,
                                RemotePreflightOutcome::Repair(_) => unreachable!(),
                            }),
                        ),
                    }
                }
            }
            DashboardIoUpdate::RepositoryRemotesRepaired {
                generation,
                retry,
                result,
            } => {
                if generation.is_some_and(|generation| {
                    generation != self.dashboard.session_preflight_generation()
                }) {
                    return;
                }
                self.dashboard.clear_notice();
                match result {
                    Ok(()) => match *retry {
                        DashboardAction::PreflightCreateSession { launch } => {
                            super::actions::start_create_session_preflight(self, *launch)
                        }
                        action => super::actions::start_session_launch(self, action),
                    },
                    Err(error) => {
                        if let Some(generation) = generation {
                            self.dashboard
                                .apply_remote_session_preflight(generation, Err(error.clone()));
                        }
                        self.dashboard.show_launch_failure(error, Some(*retry));
                    }
                }
            }
            DashboardIoUpdate::RenameSession {
                session_id,
                title,
                result,
            } => match result {
                Ok(title) => {
                    if let Some(session) = self.controller.state.sessions.get_mut(&session_id) {
                        session.session_title_override = Some(title.clone());
                        session.updated_at = chrono::Utc::now().to_rfc3339();
                    }
                    self.dashboard.set_state(self.controller.state.clone());
                    self.dashboard
                        .set_notice(format!("Renamed session to {title}"));
                }
                Err(error) => {
                    self.dashboard
                        .set_notice(format!("Rename failed for {title}: {error}"));
                }
            },
            DashboardIoUpdate::ContainerSettings { session_id, result } => match result {
                Ok(controller) => {
                    self.controller = controller;
                    self.dashboard.set_config(self.controller.config.clone());
                    self.dashboard.set_state(self.controller.state.clone());
                    self.refresh_chat_context();
                    self.dashboard.set_notice(format!(
                        "Container settings saved for {}; applies when it is next recreated.",
                        short_id(&session_id)
                    ));
                }
                Err(error) => self.dashboard.set_notice(format!(
                    "Container settings failed for {}: {error}",
                    short_id(&session_id)
                )),
            },
            DashboardIoUpdate::TargetReadiness {
                generation,
                target_id,
                result,
            } => {
                self.dashboard
                    .apply_target_readiness(generation, target_id, result);
            }
            DashboardIoUpdate::TargetTest { target_id, result } => {
                self.target_test_cancel = None;
                self.dashboard.apply_target_test(target_id, result);
            }
            DashboardIoUpdate::ConfigRename { what, result } => match result {
                Ok(controller) => {
                    self.controller = controller;
                    self.dashboard.set_config(self.controller.config.clone());
                    self.dashboard.set_state(self.controller.state.clone());
                    self.refresh_chat_context();
                    self.refresh_poll_targets();
                    self.request_quota_refresh();
                    self.dashboard.set_notice(format!("Renamed {what}."));
                }
                Err(error) => self
                    .dashboard
                    .set_notice(format!("Could not rename {what}: {error}")),
            },
            DashboardIoUpdate::ConfigReloaded(result) => {
                self.config_reload_in_flight = false;
                match result {
                    Ok(controller) => {
                        self.controller = controller;
                        self.dashboard.set_config(self.controller.config.clone());
                        self.dashboard.set_state(self.controller.state.clone());
                        self.refresh_chat_context();
                        self.refresh_poll_targets();
                    }
                    Err(error) => self
                        .dashboard
                        .set_notice(format!("Could not reload configuration: {error}")),
                }
            }
            DashboardIoUpdate::WebAccess { generation, access } => {
                if generation == self.web_request_generation {
                    self.dashboard.apply_web_access(access);
                }
            }
            DashboardIoUpdate::WebListeners { generation, result } => {
                if generation == self.web_request_generation {
                    self.dashboard.apply_web_listeners(result);
                }
            }
            DashboardIoUpdate::WebAccessError { generation, error } => {
                if generation == self.web_request_generation {
                    self.dashboard.apply_web_error(error);
                }
            }
            DashboardIoUpdate::SetupDiscovered { generation, result } => {
                self.dashboard.setup_discovered(generation, result)
            }
            DashboardIoUpdate::SetupSaved { generation, result } => {
                let result = result.map(Config::with_local_targets);
                if let Ok(config) = &result {
                    self.controller.config = config.clone();
                    self.refresh_chat_context();
                    self.request_quota_refresh();
                    self.refresh_poll_targets();
                }
                self.dashboard.setup_saved(generation, result);
            }
            DashboardIoUpdate::ReviewSettingsChoices {
                generation,
                profile_id,
                model,
                choices,
            } => {
                self.dashboard.apply_review_settings_choices(
                    generation,
                    &profile_id,
                    model.as_deref(),
                    review_settings_choices(choices),
                );
            }
            DashboardIoUpdate::ReviewSettingsDiscovered {
                generation,
                profile_id,
                model,
                result,
            } => {
                if self.dashboard.apply_review_settings_discovery(
                    generation,
                    &profile_id,
                    model.as_deref(),
                    result,
                ) {
                    self.review_discovery_cancel = None;
                }
            }
            DashboardIoUpdate::SpinnerStyleSaved { result } => match result {
                Ok(config) => {
                    let style = config.spinner;
                    self.dashboard.finish_spinner_style_save();
                    self.controller.config = config.clone();
                    self.dashboard.set_config(config);
                    self.refresh_chat_context();
                    self.dashboard.set_notice(format!(
                        "Spinner: {style}. F2 → Next spinner style to change it."
                    ));
                }
                Err(error) => {
                    self.dashboard.finish_spinner_style_save();
                    self.dashboard
                        .set_failure_notice(format!("Could not save spinner style: {error}"));
                }
            },
            DashboardIoUpdate::ClipboardWritten(result) => {
                if let Err(error) = result {
                    self.dashboard.set_failure_notice(format!(
                        "Copy to the system clipboard failed: {error}"
                    ));
                }
            }
            DashboardIoUpdate::ClipboardText(result) => {
                self.clipboard_read_in_flight = false;
                match result {
                    Ok(text) => self.dashboard.handle_paste(&text),
                    Err(error) => self.dashboard.set_notice(format!("Paste failed: {error}")),
                }
            }
            DashboardIoUpdate::DetachedSessionState { session_id, result } => match result {
                Ok(()) => {
                    self.draft_save_failures.remove(&session_id);
                }
                Err(error) => {
                    let message = format!(
                        "Could not confirm saved draft and read status for {}: {error}",
                        short_id(&session_id)
                    );
                    self.draft_save_failures.insert(session_id, message.clone());
                    self.dashboard.set_notice(message);
                }
            },
            DashboardIoUpdate::ReadReceipt { session_id, result } => {
                self.finish_read_receipt(session_id, result);
            }
            DashboardIoUpdate::CreatedBundle { result } => match *result {
                Ok(created) => {
                    self.controller.config = created.config;
                    let followup = self
                        .dashboard
                        .apply_created_bundle(self.controller.config.clone(), &created.bundle_id);
                    if let DashboardAction::ResolveAwsResourceOptions {
                        target_template_ids,
                    } = followup
                    {
                        self.resolve_aws_resource_options(target_template_ids);
                    }
                }
                Err(error) => {
                    self.dashboard.fail_bundle_creation(&error);
                }
            },
            DashboardIoUpdate::ImportedSessionApplied { result } => match *result {
                Ok(applied) => {
                    let session_id = applied.session.id.clone();
                    self.controller
                        .config
                        .bundles
                        .insert(applied.bundle_id, applied.bundle);
                    self.controller
                        .state
                        .sessions
                        .insert(session_id.clone(), applied.session);
                    self.dashboard.set_config(self.controller.config.clone());
                    self.dashboard.set_state(self.controller.state.clone());
                    self.resolve_project_sources();
                    self.refresh_poll_targets();
                    self.dashboard.set_notice(format!(
                        "Imported {} session {}.",
                        applied.harness, applied.native_session_id
                    ));
                    // Import completion can arrive after a tab switch. Keep
                    // the imported record globally visible, but only open
                    // the resume modal in the workspace that owns it.
                    if self.session_in_active_workspace(&session_id)
                        && let DashboardAction::ResolveAwsResourceOptions {
                            target_template_ids,
                        } = self.dashboard.begin_resume_for(&session_id)
                    {
                        self.resolve_aws_resource_options(target_template_ids);
                    }
                }
                Err(error) => self.dashboard.set_notice(format!("Import failed: {error}")),
            },
            DashboardIoUpdate::LifecycleReloaded(reloaded) => {
                self.apply_lifecycle_reloaded(*reloaded)
            }
            DashboardIoUpdate::LifecycleCancellation { session_id, result } => {
                if let Err(error) = result {
                    self.dashboard.set_failure_notice(format!(
                        "Could not cancel operation for {}: {error}",
                        short_id(&session_id)
                    ));
                }
            }
            DashboardIoUpdate::MovePrepared {
                session_id,
                request_id,
                result,
            } => match result {
                Ok(preparation) => {
                    self.dashboard
                        .apply_move_preparation(request_id, preparation);
                }
                Err(error) => {
                    self.dashboard
                        .set_move_preparation_failed(&session_id, request_id, error);
                }
            },
            DashboardIoUpdate::CheckpointArchiveSizes { generation, sizes } => {
                if generation == self.checkpoint_archive_generation {
                    self.dashboard.apply_checkpoint_archive_sizes(sizes);
                }
            }
            DashboardIoUpdate::WorkerDiagnosis {
                session_id,
                episode_id,
                result,
            } => self.apply_worker_diagnosis(session_id, episode_id, result),
            DashboardIoUpdate::MountCompletions { prefix, result } => match result {
                Ok(candidates) => self
                    .dashboard
                    .apply_mount_source_completions(&prefix, candidates),
                Err(error) => self
                    .dashboard
                    .set_notice(format!("Path completion failed: {error}")),
            },
            DashboardIoUpdate::MountValidation {
                context,
                source,
                result,
            } => {
                let action = self
                    .dashboard
                    .apply_resolved_mount_source(&context, &source, result);
                super::actions::start_move_preparation(self, action);
            }
            DashboardIoUpdate::SessionMountValidation {
                generation,
                launch,
                result,
            } => {
                if generation != self.dashboard.session_preflight_generation() {
                    if let Err(error) = result {
                        tracing::warn!(%error, "cancelled mount preflight failed");
                    }
                    return;
                }
                match result {
                    Ok(None) => match *launch {
                        DashboardAction::PreflightResumeRepositories { launch } => {
                            if let Err(error) =
                                super::actions::start_resume_repository_preflight(self, launch)
                            {
                                self.dashboard.set_notice(format!(
                                    "Could not check checkpoint repositories: {error:#}"
                                ));
                            }
                        }
                        launch => {
                            if matches!(&launch, DashboardAction::CreateSession { .. }) {
                                super::actions::start_create_session_preflight(self, launch);
                            } else {
                                self.dashboard.finish_session_mount_preflight();
                                super::actions::start_session_launch(self, launch);
                            }
                        }
                    },
                    Ok(Some((source, error))) => {
                        self.dashboard
                            .apply_session_mount_preflight_failure(&source, error);
                    }
                    Err(error) => {
                        let error = format!("Could not check attached directories: {error}");
                        self.dashboard
                            .apply_remote_session_preflight(generation, Err(error.clone()));
                        self.dashboard.set_notice(error);
                    }
                }
            }
            DashboardIoUpdate::ResumeRepositoryPreflight {
                generation,
                launch,
                submitted_repository_id,
                result,
            } => {
                if generation != self.dashboard.session_preflight_generation() {
                    if let Err(error) = *result {
                        tracing::warn!(%error, "cancelled repository preflight failed");
                    }
                    return;
                }
                match *result {
                    Ok(applied) => {
                        if let Some(config) = applied.config {
                            self.controller.config = config.clone();
                            self.dashboard.set_config(config);
                        }
                        match applied.preflight {
                            ResumeRepositorySourcePreflight::Ready(receipt) => {
                                self.dashboard.finish_resume_repository_preflight();
                                super::actions::start_preflighted_session_launch(
                                    self, *launch, receipt,
                                );
                            }
                            ResumeRepositorySourcePreflight::ConvertingRawCheckout {
                                receipt,
                                preview,
                            } => {
                                self.dashboard
                                    .show_raw_conversion_confirmation(*launch, receipt, *preview);
                            }
                            ResumeRepositorySourcePreflight::RepositoryMoved(mismatch) => {
                                if submitted_repository_id.as_deref()
                                    == Some(mismatch.repository_id.as_str())
                                {
                                    self.dashboard.apply_repository_origin_failure(
                                        &mismatch.repository_id,
                                        format!(
                                            "That origin does not contain checkpoint base {}.",
                                            mismatch.missing_commit
                                        ),
                                    );
                                } else {
                                    self.dashboard.show_repository_origin_dialog(
                                        mismatch.session_id,
                                        mismatch.repository_id,
                                        mismatch.missing_commit,
                                        mismatch.archived_origin,
                                        mismatch.configured_origin,
                                        *launch,
                                    );
                                }
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(repository_id) = submitted_repository_id {
                            self.dashboard
                                .apply_repository_origin_failure(&repository_id, error);
                        } else {
                            self.dashboard.set_notice(format!(
                                "Could not check checkpoint repositories: {error}"
                            ));
                        }
                    }
                }
            }
            DashboardIoUpdate::ContainerPathResolved { context, result } => {
                self.dashboard.container_path_resolved(&context, result)
            }
            DashboardIoUpdate::SetupPathResolved {
                generation,
                draft,
                path,
                value,
                result,
            } => self
                .dashboard
                .setup_path_resolved(generation, &draft, &path, &value, result),
            DashboardIoUpdate::ProjectValidation {
                context,
                directory,
                result,
            } => self
                .dashboard
                .apply_resolved_project_directory(&context, &directory, result),
        }
    }

    fn apply_create_session_update(&mut self, update: DashboardCreateSessionUpdate) {
        match update {
            DashboardCreateSessionUpdate::RemoteRepair {
                bundle_id,
                repairs,
                retry,
            } => {
                self.dashboard.clear_notice();
                self.dashboard
                    .show_remote_repair_confirmation(bundle_id, repairs, *retry);
            }
            DashboardCreateSessionUpdate::Registered(registered) => {
                let registered = *registered;
                let session_id = registered.session.id.clone();
                if let Some((host, size)) = registered.remembered_container_size {
                    self.controller.state.remember_container_size(&host, size);
                }
                self.controller
                    .state
                    .sessions
                    .insert(session_id.clone(), registered.session);
                self.dashboard.set_state(self.controller.state.clone());
                self.dashboard.select_active_session(&session_id);
                self.resolve_project_sources();
                self.dashboard.begin_session_operation(
                    session_id.clone(),
                    SessionOperationKind::Launching,
                    None,
                );
                // The next thing the person does with a launching session is
                // write its first message, so the keyboard starts where the
                // type-ahead composer is.
                self.dashboard.focus_prompt();
                self.dashboard
                    .set_notice(format!("Launching {}…", short_id(&session_id)));
                self.lifecycle_operations.insert(
                    session_id,
                    ActiveLifecycleOperation {
                        cancelled: registered.cancelled,
                        kind: SessionOperationKind::Launching,
                        retry_launch: Some(registered.retry_launch),
                    },
                );
            }
            DashboardCreateSessionUpdate::Failed {
                error,
                retry_launch,
            } => {
                self.dashboard
                    .show_launch_failure(error, Some(*retry_launch));
            }
        }
    }

    fn apply_lifecycle_reloaded(&mut self, reloaded: LifecycleReloaded) {
        let LifecycleReload { update, operation } = reloaded.reload;
        let session_id = update.session_id;
        let loaded = match reloaded.result {
            Ok(loaded) => loaded,
            Err(error) => {
                self.dashboard
                    .set_notice(format!("Could not reload completed operation: {error}"));
                return;
            }
        };
        self.controller = loaded;
        self.dashboard.set_state(self.controller.state.clone());
        self.resolve_project_sources();
        // A lifecycle may finish after the user changed tabs. Its durable
        // record still belongs in the global controller, but completion must
        // not move the visible selection or replace another workspace's chat.
        let focus_session = self.session_in_active_workspace(&session_id);
        if update.result.is_ok() {
            self.drop_warm_chat_for(&session_id);
        }
        match update.result {
            Ok(LifecycleSuccess::Created) => {
                if focus_session {
                    self.dashboard.finish_new_session(&session_id);
                }
                self.dashboard
                    .set_notice(format!("Session {} is ready", short_id(&session_id)));
                self.request_quota_refresh();
            }
            Ok(LifecycleSuccess::Resumed {
                profile_id,
                target_id,
            }) => {
                // Seed the conversation from the tail rather than from a
                // transcript shipped back through the daemon reply. The view
                // keeps `TAIL_SEED_ITEMS` and discards everything before it,
                // so reading the whole projection was work proportional to
                // history for a result that was thrown away.
                if focus_session {
                    self.request_transcript_tail_seed(&session_id);
                    self.open_chat_session(&session_id);
                }
                self.dashboard.set_notice(format!(
                    "Resumed {} with {profile_id} on {target_id}",
                    short_id(&session_id)
                ));
                self.request_quota_refresh();
            }
            Ok(LifecycleSuccess::Moved(outcome)) => {
                if focus_session {
                    self.request_transcript_tail_seed(&session_id);
                    self.open_chat_session(&session_id);
                }
                let destination = format!("{}/{}", outcome.profile_id, outcome.target_template_id);
                self.dashboard
                    .set_notice(if outcome.outcome == "unchanged" {
                        format!(
                            "Move of {} unchanged on {destination} (operation {})",
                            short_id(&session_id),
                            outcome.operation_id
                        )
                    } else {
                        format!(
                            "Moved {} to {destination}; ready and idle (operation {})",
                            short_id(&session_id),
                            outcome.operation_id
                        )
                    });
                self.request_quota_refresh();
            }
            Ok(LifecycleSuccess::Closed) => {
                self.dashboard
                    .set_notice(format!("Stopped {}", short_id(&session_id)));
            }
            Ok(LifecycleSuccess::ForceStopped) => self.dashboard.set_notice(format!(
                "Force-stopped {} at its latest recovery archive",
                short_id(&session_id)
            )),
            Ok(LifecycleSuccess::DestroyedStopped) => self.dashboard.set_notice(format!(
                "Permanently destroyed stopped session {}",
                short_id(&session_id)
            )),
            Ok(LifecycleSuccess::ForceDestroyed) => self.dashboard.set_notice(format!(
                "Permanently destroyed session {}",
                short_id(&session_id)
            )),
            Err(error) => {
                if operation
                    .as_ref()
                    .is_some_and(|operation| operation.kind == SessionOperationKind::Stopping)
                {
                    self.dashboard.show_close_failure(session_id.clone(), error);
                } else if operation
                    .as_ref()
                    .is_some_and(|operation| operation.kind == SessionOperationKind::Launching)
                {
                    let retry = operation
                        .and_then(|operation| operation.retry_launch)
                        .filter(|_| !self.controller.state.sessions.contains_key(&session_id));
                    self.dashboard.show_launch_failure(error, retry);
                } else {
                    let label = operation
                        .as_ref()
                        .map_or("Operation", |operation| operation.kind.label());
                    self.dashboard
                        .set_failure_notice(format!("{label} failed: {error}"));
                }
            }
        }
        self.refresh_poll_targets();
    }

    fn apply_worker_diagnosis(
        &mut self,
        session_id: String,
        episode_id: u64,
        result: std::result::Result<Option<String>, String>,
    ) {
        let completion = self.worker_diagnoses.finish(&session_id, episode_id);
        if let Some(error) = completion.display_error {
            let mut message = format!("relay unreachable: {error}");
            match &result {
                Ok(Some(diagnosis)) => {
                    message.push_str("; ");
                    message.push_str(diagnosis);
                }
                Ok(None) => {}
                Err(failure) => {
                    message.push_str("; worker diagnostics failed: ");
                    message.push_str(failure);
                }
            }
            self.dashboard
                .set_notice(format!("Session {}: {message}", short_id(&session_id)));
        } else if let Err(error) = &result {
            tracing::warn!(%session_id, "stale worker diagnosis task failed: {error}");
        }
        if let Some(restart_episode) = completion.restart_episode {
            crate::pollers::spawn_worker_diagnosis(
                &self.controller,
                session_id,
                restart_episode,
                self.dashboard_io_tx.clone(),
                self.critical_operations.clone(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_controller::controller::create_quick_bundle_in_config as create_quick_bundle;
    use mj_core::config::{HarnessKind, ProjectRepository};

    #[test]
    fn setup_save_refuses_to_remove_an_active_sessions_target_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let mut original = Config::default();
        original
            .targets
            .insert("podman".into(), mj_core::config::TargetTemplate::LocalBare);
        original.save_to(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let session = lifecycle_session("session-1", "default", SessionState::Running);
        let state = State {
            sessions: BTreeMap::from([(session.id.clone(), session)]),
            ..State::default()
        };
        let mut updated = original.clone();
        updated.targets.clear();
        let error = save_setup_at(
            &path,
            &serde_json::to_string(&original).unwrap(),
            &serde_json::to_string(&updated).unwrap(),
            &state,
        )
        .unwrap_err();
        assert!(error.to_string().contains("active session"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // An unrelated preference remains editable even while a session needs repair.
        updated = original.clone();
        updated.advanced.show_stopped_sessions = true;
        save_setup_at(
            &path,
            &serde_json::to_string(&original).unwrap(),
            &serde_json::to_string(&updated).unwrap(),
            &state,
        )
        .unwrap();
        assert!(
            Config::load_from(&path)
                .unwrap()
                .advanced
                .show_stopped_sessions
        );
    }

    #[test]
    fn settings_can_override_an_implicit_local_target_without_a_setup_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = Config::default().with_local_targets();
        let mut edited = original.clone();
        if let mj_core::config::TargetTemplate::LocalDocker { container } =
            edited.targets.get_mut("docker").unwrap()
        {
            container.image = "example.test/custom:latest".into();
        }
        let saved = save_setup_at(
            &path,
            &serde_json::to_string(&original).unwrap(),
            &serde_json::to_string(&edited).unwrap(),
            &State::default(),
        )
        .unwrap();
        assert_eq!(saved.targets["docker"], edited.targets["docker"]);
        assert_eq!(
            Config::load_from(&path).unwrap().targets["docker"],
            edited.targets["docker"]
        );
    }

    #[test]
    fn setup_save_merges_unrelated_edits_and_refuses_conflicts_or_invalid_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = Config::default();
        original.save_to(&path).unwrap();
        let mut edited = original.clone();
        edited.sessions_side = mj_core::config::SessionsSide::Right;
        edited.theme = mj_core::config::UiTheme::Light;
        Config::update_to(&path, |current| {
            current.advanced.show_stopped_sessions = true;
            Ok(())
        })
        .unwrap();
        let original_json = serde_json::to_string(&original).unwrap();
        let saved = save_setup_at(
            &path,
            &original_json,
            &serde_json::to_string(&edited).unwrap(),
            &State::default(),
        )
        .unwrap();
        assert_eq!(saved.sessions_side, mj_core::config::SessionsSide::Right);
        assert_eq!(saved.theme, mj_core::config::UiTheme::Light);
        assert!(saved.advanced.show_stopped_sessions);
        assert_eq!(Config::load_from(&path).unwrap(), saved);

        let mut conflicting = original.clone();
        conflicting.phone.bind = "127.0.0.1:1234".parse().unwrap();
        Config::update_to(&path, |current| {
            current.phone.bind = "127.0.0.1:5678".parse().unwrap();
            Ok(())
        })
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(
            save_setup_at(
                &path,
                &original_json,
                &serde_json::to_string(&conflicting).unwrap(),
                &State::default(),
            )
            .unwrap_err()
            .to_string()
            .contains("another client")
        );
        let mut invalid = original.clone();
        invalid.review.profile = Some("missing".into());
        assert!(
            save_setup_at(
                &path,
                &original_json,
                &serde_json::to_string(&invalid).unwrap(),
                &State::default(),
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn asynchronous_saves_leave_the_blocking_pool_available_for_connection_metadata() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (tracker, _) = CriticalOperationTracker::new();
            let (updates, mut results) = tokio::sync::mpsc::unbounded_channel();
            let (ready, mut started) = tokio::sync::mpsc::unbounded_channel();
            let release = Arc::new(tokio::sync::Notify::new());
            let mut tasks = Vec::new();
            for id in 0..64 {
                let ready = ready.clone();
                let release = release.clone();
                tasks.push(spawn_critical_async(
                    tracker.clone(),
                    format!("saving draft {id}"),
                    updates.clone(),
                    Duration::from_secs(2),
                    async move {
                        // The real connection needs a blocking metadata read.
                        tokio::task::spawn_blocking(|| ()).await?;
                        let released = release.notified();
                        tokio::pin!(released);
                        released.as_mut().enable();
                        ready.send(())?;
                        released.await;
                        Ok(())
                    },
                    move |result| DashboardIoUpdate::DetachedSessionState {
                        session_id: id.to_string(),
                        result,
                    },
                ));
            }
            tokio::time::timeout(Duration::from_secs(1), async {
                for _ in 0..64 {
                    started.recv().await.unwrap();
                }
                // Chat preparation and other UI work can still use the pool.
                assert_eq!(
                    tokio::task::spawn_blocking(|| "chat ready").await.unwrap(),
                    "chat ready"
                );
            })
            .await
            .expect("network waits must not starve blocking work");
            release.notify_waiters();
            for task in tasks {
                task.await.unwrap();
            }
            for _ in 0..64 {
                assert!(matches!(
                    results.recv().await,
                    Some(DashboardIoUpdate::DetachedSessionState { result: Ok(()), .. })
                ));
            }
            assert!(
                tracker.blockers().is_empty(),
                "saves must release quit blockers"
            );
        });
        runtime.shutdown_timeout(Duration::from_secs(1));
    }

    #[tokio::test]
    async fn unresponsive_save_reports_uncertain_durability_and_releases_quit() {
        let (tracker, _) = CriticalOperationTracker::new();
        let (updates, mut results) = tokio::sync::mpsc::unbounded_channel();
        let task = spawn_critical_async(
            tracker.clone(),
            "saving draft",
            updates,
            Duration::from_millis(20),
            std::future::pending::<Result<()>>(),
            |result| DashboardIoUpdate::DetachedSessionState {
                session_id: "muse".into(),
                result,
            },
        );
        assert_eq!(tracker.blockers().len(), 1);
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        let Some(DashboardIoUpdate::DetachedSessionState {
            result: Err(error), ..
        }) = results.recv().await
        else {
            panic!("missing save failure");
        };
        assert!(error.contains("did not acknowledge"));
        assert!(error.contains("may still complete"));
        assert!(tracker.blockers().is_empty());
    }

    #[test]
    fn quick_github_bundle_uses_collision_suffix_and_reuses_matching_source() {
        let mut config = Config::default();
        config.bundles.insert(
            "app".into(),
            ProjectBundle {
                primary_repo: "app".into(),
                repositories: vec![ProjectRepository {
                    id: "app".into(),
                    github: Some("other/app".into()),
                    local: None,
                    destination: "app".into(),
                    git_ref: None,
                }],
            },
        );

        let created =
            create_quick_bundle(&mut config, "https://github.com/example/app.git").unwrap();
        assert_eq!(created, "app-2");
        assert_eq!(
            create_quick_bundle(&mut config, "example/app").unwrap(),
            "app-2"
        );
        assert_eq!(config.bundles.len(), 2);
    }

    const LIFECYCLE_RELOAD_CHILD: &str = "MJ_TEST_LIFECYCLE_RELOAD_CHILD";

    /// A completed lifecycle is the moment a freshly started container session
    /// first appears, so the reload it schedules is exactly when another
    /// workspace's live sessions would flood the pane. The reloaded controller
    /// must carry this workspace's live sessions and the global stopped
    /// history, and nothing else.
    #[tokio::test]
    async fn a_lifecycle_reload_keeps_sessions_from_all_workspaces() {
        if std::env::var_os(LIFECYCLE_RELOAD_CHILD).is_none() {
            // MJ_DATA_DIR is process-global, so the database-backed half runs
            // alone in an exact child with its own store.
            let directory = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "dashboard::io::tests::a_lifecycle_reload_keeps_sessions_from_all_workspaces",
                    "--nocapture",
                ])
                .env(LIFECYCLE_RELOAD_CHILD, "1")
                .env("MJ_DATA_DIR", directory.path())
                .env("MJ_CONFIG_DIR", directory.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "isolated lifecycle reload failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let _writer = mj_controller::database::install_isolated_test_writer();

        let mut config = Config::default();
        config.profiles.insert(
            "codex".into(),
            mj_core::config::HarnessProfile {
                enabled: true,
                kind: HarnessKind::Codex,
                home: PathBuf::from("/home/dev/.codex"),
                environment: BTreeMap::new(),
                context_window_bytes: None,
                guardian_review_model: None,
            },
        );
        config.bundles.insert(
            "project".into(),
            ProjectBundle {
                primary_repo: "project".into(),
                repositories: vec![ProjectRepository {
                    id: "project".into(),
                    github: Some("owner/project".into()),
                    local: None,
                    destination: PathBuf::from("project"),
                    git_ref: None,
                }],
            },
        );
        config.targets.insert(
            "podman".into(),
            mj_core::config::TargetTemplate::LocalPodman {
                container: mj_core::config::ContainerTemplate {
                    image: "example.invalid/hel-test:latest".into(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: BTreeMap::new(),
                    workspace_storage: Default::default(),
                },
            },
        );
        config.save().unwrap();

        let database = mj_controller::database::database_path();
        let alpha = mj_controller::database::create_workspace_at(&database, "alpha").unwrap();
        let beta = mj_controller::database::create_workspace_at(&database, "beta").unwrap();
        mj_controller::database::save_session(&lifecycle_session(
            "session-alpha-live",
            &alpha.id,
            SessionState::Running,
        ))
        .unwrap();
        mj_controller::database::save_session(&lifecycle_session(
            "session-alpha-stopped",
            &alpha.id,
            SessionState::Stopped,
        ))
        .unwrap();
        mj_controller::database::save_session(&lifecycle_session(
            "session-beta-live",
            &beta.id,
            SessionState::Running,
        ))
        .unwrap();

        let (updates_tx, mut updates_rx) =
            tokio::sync::mpsc::unbounded_channel::<DashboardIoUpdate>();
        spawn_lifecycle_reload(
            LifecycleReload {
                update: LifecycleUpdate {
                    session_id: "session-beta-live".into(),
                    result: Ok(LifecycleSuccess::Created),
                    deferred_cleanup: false,
                },
                operation: None,
            },
            beta.id.clone(),
            "client-1".into(),
            updates_tx,
        );

        let DashboardIoUpdate::LifecycleReloaded(reloaded) =
            updates_rx.recv().await.expect("the reload reports back")
        else {
            panic!("the reload reports through LifecycleReloaded");
        };
        let loaded = reloaded.result.expect("the reload succeeds");
        let ids = loaded.state.sessions.keys().collect::<BTreeSet<_>>();
        assert_eq!(
            ids,
            BTreeSet::from([
                &"session-beta-live".to_owned(),
                &"session-alpha-live".to_owned(),
                &"session-alpha-stopped".to_owned()
            ]),
            "the sidebar keeps all live sessions and stopped history"
        );
    }

    fn lifecycle_session(id: &str, workspace_id: &str, state: SessionState) -> SessionRecord {
        SessionRecord {
            mjolnir_subagents: None,
            create_managed_worktree: None,
            workspace_id: workspace_id.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: id.into(),
            title: id.into(),
            harness_kind: HarnessKind::Codex,
            last_profile: "codex".into(),
            bundle_id: "project".into(),
            project_directory: None,
            managed_worktree: None,
            target_template_id: "podman".into(),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state,
            target: None,
            native_session_id: None,
            acp_session_title: None,
            session_title_override: None,
            created_at: "2026-09-04T00:00:00Z".into(),
            updated_at: "2026-09-04T00:00:00Z".into(),
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        }
    }
}

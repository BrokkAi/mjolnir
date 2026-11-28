//! The daemon half of the documented HTTP API.
//!
//! `server::api` serves the `/api/v1` routes against the
//! [`SubagentBackend`](crate::server::api::SubagentBackend) trait, so its
//! route tests can use a fake. This module is the daemon's implementation.
//!
//! Every database read runs on `spawn_blocking`: the web server's handlers run
//! on the async runtime, and a synchronous SQLite read on that thread would
//! stall unrelated requests.

use mj_core::targets::SshTrafficClass;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::v1::schema::{ContentBlock, TextContent};
use anyhow::{Context, Result, anyhow, bail, ensure};

use mj_core::state::{MaterializedExecutionState, SessionState};
use mj_core::subagent::{ReportState, bounded_report};

use mj_controller::quota::ProfileQuota;

use crate::server::api::{
    ApiFailure, BundleExport, ExportError, ExportPatch, PurgeUntrackedFailure,
    PurgeUntrackedRequest, PurgeUntrackedResponse, PushedBranch, StartFollowup, StartStatus,
    SubagentBackend, TranscriptPage, TurnSpan, TurnState, TurnSummary, UploadBody,
};
use crate::server::{
    ActionOutcome, BackgroundTaskStopFailure, BundleFailure, CreateBundleResponse,
    MAX_HISTORY_MATCHES, PreflightFailure, PreflightNew, PreflightResume, ViewerClientState,
    ViewerPromptHistory,
};
use mj_checkpoint::archive::{FetchMode, FetchedPath};
use mj_client::runtime_feed::RuntimeProjection;
use mj_client::session::{BoxFuture, SessionControl, SessionHandle, new_command_id};
use mj_controller::controller::{
    Controller, QuickBundleCreation, QuickBundleFailure, SessionExportLayout,
};
use mj_controller::targets::{
    self, CancellableProcessExecutor, CommandExecutor, CommandOutput, agent_working_directory_at,
    target_join,
};
use mj_core::path_completion::{CompletionHost, CompletionKind, PathCompletion};
use mj_core::refusal::Refusal;
use mj_core::relay::RelayCommand;
use mj_core::state::{
    ContainerSettingsChange, ContainerUpgradePreview, MovePreparation, MoveSelection,
    SessionResourceAllocation,
};
use mj_core::suspend_consent::SuspendPreflight;
use mj_core::targets::AdditionalMount;

mod fetch_transfer;
mod oracle;
mod subagent_input;

use crate::daemon::RuntimeState;
use mj_client::daemon::{WikiHitTranscript, WikiRestoreRequest, WikiSearchPage, WikiSessionInfo};

/// Legacy constructor input. Startup state now comes from the durable owner;
/// retain the argument so existing backend callers do not need a second path.
pub type SessionStateSource = Arc<dyn Fn(&str) -> Option<SessionState> + Send + Sync>;

/// What the export operations need from the daemon beyond a session's live
/// actor: the durable record, and a checkpoint on demand.
///
/// It is a trait rather than the daemon runtime itself so the backend can be
/// built in a test without one. Startup admission and status also go through
/// this runtime, keeping their durable owner shared by every control surface.
pub trait ExportRuntime: Send + Sync {
    /// Fetch `path` of `session` into `parent` entirely on one remote SSH host,
    /// when both sessions run there. `Ok(None)` means they do not, so the caller
    /// moves the payload through the daemon. On success: the destination
    /// worker's `store-fetch` output.
    fn try_fetch_same_host(
        &self,
        _parent: String,
        _session: String,
        _path: PathBuf,
    ) -> BoxFuture<'_, std::result::Result<Option<Vec<u8>>, ExportError>> {
        Box::pin(async { Ok(None) })
    }

    fn dashboard_backend(self: Arc<Self>) -> Option<Arc<dyn crate::server::api::DashboardBackend>> {
        None
    }

    fn stop_viewer_background_task(
        &self,
        _session_id: String,
        _background_task_id: String,
    ) -> BoxFuture<'_, Result<(), BackgroundTaskStopFailure>> {
        Box::pin(async { Err(BackgroundTaskStopFailure::SessionUnavailable) })
    }

    fn purge_untracked(
        self: Arc<Self>,
        _request: PurgeUntrackedRequest,
        _cancelled: Arc<std::sync::atomic::AtomicBool>,
    ) -> BoxFuture<'static, Result<PurgeUntrackedResponse, PurgeUntrackedFailure>> {
        Box::pin(async {
            Err(PurgeUntrackedFailure::Internal(
                "controller unavailable".into(),
            ))
        })
    }

    fn resolve_suspend_blockers(
        self: Arc<Self>,
        _session_id: String,
    ) -> BoxFuture<
        'static,
        Result<mj_core::suspend_consent::ResolveSuspendBlockersResponse, anyhow::Error>,
    > {
        Box::pin(async { Err(anyhow!("controller unavailable")) })
    }
    fn suspend_preflight(
        self: Arc<Self>,
        _session_id: String,
        _dry_run: bool,
        _without_checkpoint: bool,
        _history_only: bool,
    ) -> BoxFuture<'static, Result<SuspendPreflight, anyhow::Error>> {
        Box::pin(async { Err(anyhow!("controller unavailable")) })
    }

    fn preview_container_upgrade(
        self: Arc<Self>,
        _session_id: String,
    ) -> BoxFuture<'static, Result<ContainerUpgradePreview, anyhow::Error>> {
        Box::pin(async { Err(anyhow!("controller unavailable")) })
    }

    fn set_container_settings(
        self: Arc<Self>,
        _session_id: String,
        _settings: ContainerSettingsChange,
    ) -> BoxFuture<'static, Result<Option<ContainerUpgradePreview>, anyhow::Error>> {
        Box::pin(async { Err(anyhow!("controller unavailable")) })
    }

    fn project_catalog(
        self: Arc<Self>,
        _refresh: bool,
        _retry: bool,
    ) -> BoxFuture<'static, Result<mj_core::project_catalog::ProjectCatalogView, anyhow::Error>>
    {
        Box::pin(async { Err(anyhow!("controller unavailable")) })
    }

    fn prepare_move(
        self: Arc<Self>,
        _selection: MoveSelection,
    ) -> BoxFuture<'static, Result<MovePreparation, anyhow::Error>> {
        Box::pin(async { Err(anyhow!("controller unavailable")) })
    }

    fn create_viewer_bundle(
        self: Arc<Self>,
        _source: String,
        _exact_sources: Option<Vec<String>>,
        _discovered: bool,
    ) -> BoxFuture<'static, Result<QuickBundleCreation, QuickBundleFailure>> {
        Box::pin(async {
            Err(QuickBundleFailure::Persistence(anyhow!(
                "controller unavailable"
            )))
        })
    }

    fn push_notice(&self, _session_id: &str, _message: String) {}
    fn workspace_admission(
        &self,
        _session_id: String,
        _cancelled: Arc<std::sync::atomic::AtomicBool>,
    ) -> BoxFuture<'_, Result<mj_controller::recovery_gate::SessionAdmission>> {
        Box::pin(async { bail!("workspace admission is unavailable") })
    }

    fn request_review(
        &self,
        _session_id: String,
        _paths: Vec<String>,
    ) -> BoxFuture<'_, Result<serde_json::Value>> {
        Box::pin(async { bail!("push review is unavailable") })
    }

    fn override_review(
        &self,
        _session_id: String,
        _path: Option<String>,
        _overrides: Vec<mj_core::subagent::ReviewOverride>,
    ) -> BoxFuture<'_, Result<serde_json::Value>> {
        Box::pin(async { bail!("push review is unavailable") })
    }

    fn set_review_baseline(
        &self,
        _session_id: String,
        _commit: String,
        _path: Option<String>,
    ) -> BoxFuture<'_, Result<serde_json::Value>> {
        Box::pin(async { bail!("push review is unavailable") })
    }

    fn subagent_config(&self) -> mj_core::config::SubagentConfig {
        Default::default()
    }

    fn quota_telemetry(&self) -> mj_controller::quota_telemetry::QuotaTelemetrySnapshot {
        Default::default()
    }

    fn published_quota_projections(&self) -> Vec<mj_core::usage::ProfileQuotaProjection> {
        Vec::new()
    }

    fn quota_telemetry_store(&self) -> mj_controller::quota_telemetry::QuotaTelemetryStore {
        mj_controller::quota_telemetry::QuotaTelemetryStore::new()
    }

    fn power_ranking(&self) -> mj_controller::power_ranking::PowerRankingSnapshot {
        mj_controller::power_ranking::PowerRankingSnapshot::baseline().clone()
    }

    fn record_placement(&self, _profile_id: &str, _category: &str) {}

    /// Whether a child is still waiting for its review's verdict. A child's
    /// handback or ended turn is not final while it is.
    fn child_review(&self, _child_session_id: String) -> BoxFuture<'_, crate::daemon::ChildReview> {
        Box::pin(async { crate::daemon::ChildReview::Closed })
    }

    fn queue_startup(
        self: Arc<Self>,
        _session_id: String,
        _group_id: String,
        _followup: StartFollowup,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async { bail!("durable startup delivery is unavailable") })
    }

    /// Hold one peer message until `session_id` finishes starting. The default
    /// has no startup queue, so a backend that can deliver the message now
    /// must override this.
    fn queue_startup_session_message(
        self: Arc<Self>,
        _session_id: String,
        _message: crate::daemon::startup_followup::QueuedSessionMessage,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async { bail!("durable startup delivery is unavailable") })
    }

    fn startup_status(&self, _session_id: String) -> BoxFuture<'_, Result<Option<StartStatus>>> {
        Box::pin(async { Ok(None) })
    }

    fn startup_context(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<(Option<String>, Option<StartStatus>)>> {
        Box::pin(async move { Ok((None, self.startup_status(session_id).await?)) })
    }

    fn dismiss_startup_status(
        &self,
        _session_id: String,
        _group_id: Option<String>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn cancel_startup(self: Arc<Self>, _session_id: String) -> BoxFuture<'static, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    /// The in-memory record for a session, or `None` when the daemon holds
    /// none.
    fn session_record(&self, session_id: &str) -> Option<mj_core::state::SessionRecord>;

    fn runtime_projection(&self) -> Result<Option<RuntimeProjection>> {
        Ok(None)
    }

    /// Whether a close the daemon admitted is still running for this session.
    ///
    /// The record alone cannot answer this: a close cancels the previous owner
    /// of the session before it writes `Closing`, so between admission and that
    /// write the record still says whatever it said. A backend without a daemon
    /// has no close in flight.
    fn close_is_requested(&self, _session_id: &str) -> bool {
        false
    }

    fn workspace_session(
        &self,
        _session_id: String,
    ) -> BoxFuture<'_, Result<mj_controller::session_manager::ManagedSessionHandle>> {
        Box::pin(async { anyhow::bail!("workspace file injection is unavailable") })
    }

    fn spawn_subagent(
        self: Arc<Self>,
        _request: mj_controller::controller::RegisterSubagentRequest,
    ) -> BoxFuture<'static, Result<mj_core::subagent::SubagentRecord>> {
        Box::pin(async { anyhow::bail!("sub-agent creation is unavailable") })
    }

    fn spawn_orchestrator(
        self: Arc<Self>,
        _parent: String,
        _request: mj_core::subagent::SubagentToolRequest,
        _prepared: Option<mj_controller::database::PreparedDelegation>,
    ) -> BoxFuture<'static, Result<mj_core::subagent::SubagentRecord>> {
        Box::pin(async { anyhow::bail!("orchestrator creation is unavailable") })
    }

    fn close_orchestrator(
        self: Arc<Self>,
        _session_id: String,
    ) -> BoxFuture<'static, Result<serde_json::Value>> {
        Box::pin(async { anyhow::bail!("orchestrator close is unavailable") })
    }

    fn close_subagent_request(
        self: Arc<Self>,
        _session_id: String,
        _parent: String,
        _request: String,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async { anyhow::bail!("durable sub-agent close admission is unavailable") })
    }

    fn close_subagent(self: Arc<Self>, _session_id: String) -> BoxFuture<'static, Result<()>> {
        Box::pin(async { anyhow::bail!("sub-agent close is unavailable") })
    }

    /// Whether a park of this child is still running. A backend without a
    /// daemon parks nothing.
    fn subagent_park_running(&self, _session_id: &str) -> bool {
        false
    }

    fn child_park_snapshot(
        &self,
        session_id: &str,
    ) -> (Option<mj_core::state::SessionRecord>, bool) {
        (self.session_record(session_id), false)
    }

    /// Stop an idle child's worker and keep everything else, once its parent
    /// has been told its turn ended (#1161).
    fn park_subagent(
        self: Arc<Self>,
        _session_id: String,
    ) -> BoxFuture<'static, Result<mj_controller::controller::ParkOutcome>> {
        Box::pin(async { anyhow::bail!("parking a sub-agent is unavailable") })
    }

    /// Start a parked child's worker again. A child that is not parked needs
    /// nothing.
    fn unpark_subagent(self: Arc<Self>, _session_id: String) -> BoxFuture<'static, Result<()>> {
        Box::pin(async { anyhow::bail!("restarting a parked sub-agent is unavailable") })
    }

    /// Refresh the daemon's shared workspace feed before a new workspace is
    /// returned to callers. Test backends without a daemon have no feed.
    fn refresh_workspaces(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }

    fn close_workspace(
        self: Arc<Self>,
        _id: String,
        _admission: mj_core::upgrade::Work,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async { bail!("workspace close is unavailable") })
    }

    fn cancel_workspace_close(&self, _id: &str) -> Result<()> {
        bail!("workspace close cancellation is unavailable")
    }

    fn list_workspace_drafts(
        &self,
    ) -> BoxFuture<'_, Result<Vec<mj_client::daemon::WorkspaceDrafts>>> {
        Box::pin(async { bail!("terminal drafts are unavailable") })
    }

    fn remove_project(&self, _id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { bail!("project removal is unavailable") })
    }

    fn session_working_context(
        &self,
        _session_id: String,
    ) -> BoxFuture<'_, Result<Option<mj_client::http::SessionWorkingContext>>> {
        Box::pin(async { bail!("session_working_context probe is unavailable") })
    }

    fn session_git_status(
        &self,
        _session_id: String,
    ) -> BoxFuture<'_, Result<Option<mj_core::local_git::SessionGitStatus>>> {
        Box::pin(async { bail!("session_git_status probe is unavailable") })
    }

    fn wiki_sync_request(&self) -> anyhow::Result<mj_client::daemon::WikiSyncStatus> {
        anyhow::bail!("SessionWiki sync is unavailable")
    }

    fn wiki_sync_watch(
        &self,
        _epoch: String,
        _after: u64,
    ) -> BoxFuture<'_, anyhow::Result<mj_client::daemon::WikiSyncStatus>> {
        Box::pin(async { anyhow::bail!("SessionWiki sync is unavailable") })
    }

    fn wiki_search(&self, _query: String, _limit: usize) -> BoxFuture<'_, Result<WikiSearchPage>> {
        Box::pin(async { anyhow::bail!("SessionWiki search is unavailable") })
    }

    fn wiki_brief(
        &self,
        _wiki_id: String,
        _max_chars: usize,
    ) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async { anyhow::bail!("SessionWiki briefings are unavailable") })
    }

    fn wiki_hits(
        &self,
        _wiki_id: String,
        _query: String,
        _context_messages: usize,
        _per_message_chars: usize,
    ) -> BoxFuture<'_, Result<Option<WikiHitTranscript>>> {
        Box::pin(async { anyhow::bail!("SessionWiki transcript hits are unavailable") })
    }

    fn wiki_session(&self, _wiki_id: String) -> BoxFuture<'_, Result<Option<WikiSessionInfo>>> {
        Box::pin(async { anyhow::bail!("SessionWiki lookups are unavailable") })
    }

    fn wiki_restore(
        self: Arc<Self>,
        _request: WikiRestoreRequest,
    ) -> BoxFuture<'static, Result<Option<mj_client::daemon::RegisteredSession>>> {
        Box::pin(async { anyhow::bail!("SessionWiki restore is unavailable") })
    }

    /// Publish a dashboard request (`mj ui`) to the attached dashboards.
    fn request_ui(
        &self,
        _body: mj_client::ui_request::UiRequestBody,
    ) -> std::result::Result<
        mj_client::ui_request::UiRequestReceipt,
        mj_client::ui_request::UiRequestRefusal,
    > {
        Err(mj_client::ui_request::UiRequestRefusal::NoDashboard)
    }
}

impl ExportRuntime for RuntimeState {
    fn try_fetch_same_host(
        &self,
        parent: String,
        session: String,
        path: PathBuf,
    ) -> BoxFuture<'_, std::result::Result<Option<Vec<u8>>, ExportError>> {
        Box::pin(fetch_transfer::try_transfer_same_ssh_host(
            parent, session, path,
        ))
    }

    fn dashboard_backend(self: Arc<Self>) -> Option<Arc<dyn crate::server::api::DashboardBackend>> {
        Some(self)
    }

    fn stop_viewer_background_task(
        &self,
        session_id: String,
        background_task_id: String,
    ) -> BoxFuture<'_, Result<(), BackgroundTaskStopFailure>> {
        Box::pin(RuntimeState::stop_viewer_background_task(
            self,
            session_id,
            background_task_id,
        ))
    }

    fn purge_untracked(
        self: Arc<Self>,
        request: PurgeUntrackedRequest,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    ) -> BoxFuture<'static, Result<PurgeUntrackedResponse, PurgeUntrackedFailure>> {
        Box::pin(async move { RuntimeState::purge_untracked(&self, request, cancelled).await })
    }

    fn resolve_suspend_blockers(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<
        'static,
        Result<mj_core::suspend_consent::ResolveSuspendBlockersResponse, anyhow::Error>,
    > {
        Box::pin(async move { RuntimeState::resolve_suspend_blockers(&self, session_id).await })
    }
    fn suspend_preflight(
        self: Arc<Self>,
        session_id: String,
        dry_run: bool,
        without_checkpoint: bool,
        history_only: bool,
    ) -> BoxFuture<'static, Result<SuspendPreflight, anyhow::Error>> {
        Box::pin(async move {
            RuntimeState::suspend_preflight(
                &self,
                session_id,
                dry_run,
                without_checkpoint,
                history_only,
            )
            .await
        })
    }

    fn preview_container_upgrade(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, Result<ContainerUpgradePreview, anyhow::Error>> {
        Box::pin(async move { RuntimeState::preview_container_upgrade(&self, session_id).await })
    }

    fn set_container_settings(
        self: Arc<Self>,
        session_id: String,
        settings: ContainerSettingsChange,
    ) -> BoxFuture<'static, Result<Option<ContainerUpgradePreview>, anyhow::Error>> {
        Box::pin(
            async move { RuntimeState::set_container_settings(&self, session_id, settings).await },
        )
    }

    fn project_catalog(
        self: Arc<Self>,
        refresh: bool,
        retry: bool,
    ) -> BoxFuture<'static, Result<mj_core::project_catalog::ProjectCatalogView, anyhow::Error>>
    {
        Box::pin(async move { RuntimeState::project_catalog(&self, refresh, retry).await })
    }

    fn prepare_move(
        self: Arc<Self>,
        selection: MoveSelection,
    ) -> BoxFuture<'static, Result<MovePreparation, anyhow::Error>> {
        Box::pin(async move { self.prepare_move_session(selection).await })
    }

    fn create_viewer_bundle(
        self: Arc<Self>,
        source: String,
        exact_sources: Option<Vec<String>>,
        discovered: bool,
    ) -> BoxFuture<'static, Result<QuickBundleCreation, QuickBundleFailure>> {
        Box::pin(async move {
            match exact_sources {
                Some(sources) => self.create_bundle_from_sources(sources, false).await,
                None if discovered => self.create_bundle_from_sources(vec![source], true).await,
                None => self.create_quick_bundle(source).await,
            }
        })
    }

    fn push_notice(&self, session_id: &str, message: String) {
        RuntimeState::push_notice(self, session_id, message);
    }
    fn workspace_admission(
        &self,
        session_id: String,
        cancelled: Arc<std::sync::atomic::AtomicBool>,
    ) -> BoxFuture<'_, Result<mj_controller::recovery_gate::SessionAdmission>> {
        Box::pin(async move {
            self.session_gate()
                .enter_foreground(
                    &session_id,
                    "workspace file upload",
                    mj_controller::recovery_gate::LifecycleOrigin::Person,
                    &cancelled,
                    &mj_controller::targets::ProcessExecutor,
                )
                .await
                .map(mj_controller::recovery_gate::SessionAdmission::Foreground)
        })
    }

    fn request_review(
        &self,
        session_id: String,
        paths: Vec<String>,
    ) -> BoxFuture<'_, Result<serde_json::Value>> {
        Box::pin(async move {
            self.review_host()
                .begin_push(
                    &session_id,
                    if paths.is_empty() {
                        crate::review_host::ReviewScope::WorkingDirectory
                    } else {
                        crate::review_host::ReviewScope::Paths(
                            paths.into_iter().map(PathBuf::from).collect(),
                        )
                    },
                )
                .await
                .map_err(|error| anyhow!("request_review refused: {}", error.0))
        })
    }

    fn override_review(
        &self,
        session_id: String,
        path: Option<String>,
        overrides: Vec<mj_core::subagent::ReviewOverride>,
    ) -> BoxFuture<'_, Result<serde_json::Value>> {
        Box::pin(async move {
            self.review_host()
                .override_push(&session_id, path, overrides)
                .await
                .map_err(|error| anyhow!("override_review refused: {}", error.0))
        })
    }

    fn set_review_baseline(
        &self,
        session_id: String,
        commit: String,
        path: Option<String>,
    ) -> BoxFuture<'_, Result<serde_json::Value>> {
        Box::pin(async move {
            self.review_host()
                .set_push_base(&session_id, commit, path)
                .await
                .map_err(|error| anyhow!("set_baseline refused: {}", error.0))
        })
    }

    fn subagent_config(&self) -> mj_core::config::SubagentConfig {
        self.with_config(|config| config.subagents.clone())
    }

    fn quota_telemetry(&self) -> mj_controller::quota_telemetry::QuotaTelemetrySnapshot {
        self.quota_telemetry.snapshot()
    }

    fn published_quota_projections(&self) -> Vec<mj_core::usage::ProfileQuotaProjection> {
        self.published_quota_projections()
    }
    fn quota_telemetry_store(&self) -> mj_controller::quota_telemetry::QuotaTelemetryStore {
        self.quota_telemetry.clone()
    }

    fn power_ranking(&self) -> mj_controller::power_ranking::PowerRankingSnapshot {
        self.power_ranking.snapshot()
    }

    fn record_placement(&self, profile_id: &str, category: &str) {
        self.quota_telemetry.record_placement(
            profile_id,
            category,
            mj_core::clock::epoch_seconds() as i64,
        );
    }

    fn child_review(&self, child_session_id: String) -> BoxFuture<'_, crate::daemon::ChildReview> {
        Box::pin(async move { RuntimeState::child_review(self, &child_session_id).await })
    }

    fn queue_startup(
        self: Arc<Self>,
        session_id: String,
        group_id: String,
        followup: StartFollowup,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            self.queue_api_followup(&session_id, group_id, followup)
                .await
        })
    }

    fn queue_startup_session_message(
        self: Arc<Self>,
        session_id: String,
        message: crate::daemon::startup_followup::QueuedSessionMessage,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            RuntimeState::queue_startup_session_message(&self, &session_id, message).await
        })
    }

    fn startup_status(&self, session_id: String) -> BoxFuture<'_, Result<Option<StartStatus>>> {
        Box::pin(load_startup_status(session_id))
    }

    fn startup_context(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<(Option<String>, Option<StartStatus>)>> {
        Box::pin(load_startup_context(session_id))
    }

    fn dismiss_startup_status(
        &self,
        session_id: String,
        group_id: Option<String>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if let Some(group_id) = group_id {
                blocking("dismiss completed startup status", move || {
                    mj_controller::database::dismiss_startup_group(&session_id, &group_id)
                })
                .await?;
            }
            Ok(())
        })
    }

    fn cancel_startup(self: Arc<Self>, session_id: String) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { self.cancel_api_followup(&session_id).await })
    }

    fn session_record(&self, session_id: &str) -> Option<mj_core::state::SessionRecord> {
        RuntimeState::session_record(self, session_id)
    }

    fn runtime_projection(&self) -> Result<Option<RuntimeProjection>> {
        self.runtime_publication().map(Some)
    }

    fn close_is_requested(&self, session_id: &str) -> bool {
        RuntimeState::close_is_requested(self, session_id)
    }

    fn workspace_session(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<mj_controller::session_manager::ManagedSessionHandle>> {
        Box::pin(async move { self.workspace_session_handle(&session_id).await })
    }

    fn spawn_subagent(
        self: Arc<Self>,
        request: mj_controller::controller::RegisterSubagentRequest,
    ) -> BoxFuture<'static, Result<mj_core::subagent::SubagentRecord>> {
        Box::pin(async move { self.start_subagent_session(request).await })
    }

    fn spawn_orchestrator(
        self: Arc<Self>,
        parent: String,
        request: mj_core::subagent::SubagentToolRequest,
        prepared: Option<mj_controller::database::PreparedDelegation>,
    ) -> BoxFuture<'static, Result<mj_core::subagent::SubagentRecord>> {
        Box::pin(async move {
            self.start_orchestrator_session(parent, request, prepared)
                .await
        })
    }

    fn close_orchestrator(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, Result<serde_json::Value>> {
        Box::pin(async move { RuntimeState::close_orchestrator(&self, session_id).await })
    }

    fn close_subagent_request(
        self: Arc<Self>,
        session_id: String,
        parent: String,
        request: String,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            RuntimeState::close_subagent_request(&self, session_id, parent, request).await
        })
    }

    fn close_subagent(self: Arc<Self>, session_id: String) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { self.suspend_session(session_id).await })
    }

    fn subagent_park_running(&self, session_id: &str) -> bool {
        RuntimeState::subagent_park_running(self, session_id)
    }

    fn child_park_snapshot(
        &self,
        session_id: &str,
    ) -> (Option<mj_core::state::SessionRecord>, bool) {
        RuntimeState::child_park_snapshot(self, session_id)
    }

    fn park_subagent(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, Result<mj_controller::controller::ParkOutcome>> {
        Box::pin(async move { RuntimeState::park_subagent(&self, session_id).await })
    }

    fn unpark_subagent(self: Arc<Self>, session_id: String) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { RuntimeState::unpark_subagent(&self, session_id).await })
    }

    fn refresh_workspaces(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(RuntimeState::refresh_workspaces(self))
    }

    fn close_workspace(
        self: Arc<Self>,
        id: String,
        admission: mj_core::upgrade::Work,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(
            async move { RuntimeState::close_workspace_with_admission(&self, id, admission).await },
        )
    }

    fn cancel_workspace_close(&self, id: &str) -> Result<()> {
        RuntimeState::cancel_workspace_close(self, id)
    }

    fn list_workspace_drafts(
        &self,
    ) -> BoxFuture<'_, Result<Vec<mj_client::daemon::WorkspaceDrafts>>> {
        Box::pin(RuntimeState::list_workspace_drafts(self))
    }

    fn remove_project(&self, id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(RuntimeState::remove_project(self, id))
    }

    fn session_working_context(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<Option<mj_client::http::SessionWorkingContext>>> {
        let controller = self.with_controller(|controller| Controller {
            config: controller.config.clone(),
            state: controller.state.clone(),
        });
        Box::pin(blocking("session_working_context probe", move || {
            if !controller.state.sessions.contains_key(&session_id) {
                return Ok(None);
            }
            let executor = mj_controller::targets::CancellableProcessExecutor::with_timeout(
                Duration::from_secs(3),
            );
            let (cwd, branch) = controller.session_working_context(&session_id, &executor)?;
            Ok(Some(mj_client::http::SessionWorkingContext { cwd, branch }))
        }))
    }

    fn session_git_status(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<Option<mj_core::local_git::SessionGitStatus>>> {
        let controller = self.with_controller(|controller| Controller {
            config: controller.config.clone(),
            state: controller.state.clone(),
        });
        Box::pin(blocking("session_git_status probe", move || {
            if !controller.state.sessions.contains_key(&session_id) {
                return Ok(None);
            }
            let executor = mj_controller::targets::CancellableProcessExecutor::with_timeout(
                Duration::from_secs(8),
            );
            controller
                .session_git_status(&session_id, &executor)
                .map(Some)
        }))
    }

    fn wiki_sync_request(&self) -> Result<mj_client::daemon::WikiSyncStatus> {
        let target = self.wiki().request_sync(false);
        let mut status = self.wiki().state();
        status.target = Some(target);
        Ok(status)
    }

    fn wiki_sync_watch(
        &self,
        epoch: String,
        after: u64,
    ) -> BoxFuture<'_, Result<mj_client::daemon::WikiSyncStatus>> {
        Box::pin(self.wiki().watch_after(epoch, after))
    }

    fn wiki_search(&self, query: String, limit: usize) -> BoxFuture<'_, Result<WikiSearchPage>> {
        Box::pin(async move { RuntimeState::wiki_search(self, query, limit).await })
    }

    fn wiki_brief(
        &self,
        wiki_id: String,
        max_chars: usize,
    ) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async move { RuntimeState::wiki_brief(self, wiki_id, max_chars).await })
    }

    fn wiki_hits(
        &self,
        wiki_id: String,
        query: String,
        context_messages: usize,
        per_message_chars: usize,
    ) -> BoxFuture<'_, Result<Option<WikiHitTranscript>>> {
        Box::pin(async move {
            RuntimeState::wiki_hits(self, wiki_id, query, context_messages, per_message_chars).await
        })
    }

    fn wiki_session(&self, wiki_id: String) -> BoxFuture<'_, Result<Option<WikiSessionInfo>>> {
        Box::pin(async move { RuntimeState::wiki_session(self, wiki_id).await })
    }

    fn wiki_restore(
        self: Arc<Self>,
        request: WikiRestoreRequest,
    ) -> BoxFuture<'static, Result<Option<mj_client::daemon::RegisteredSession>>> {
        Box::pin(async move {
            // The HTTP API has no request-scoped cancellation; the daemon's
            // shutdown drain cancels the queue's own token directly.
            self.restore_wiki_session(request, &tokio_util::sync::CancellationToken::new())
                .await
        })
    }

    fn request_ui(
        &self,
        body: mj_client::ui_request::UiRequestBody,
    ) -> std::result::Result<
        mj_client::ui_request::UiRequestReceipt,
        mj_client::ui_request::UiRequestRefusal,
    > {
        RuntimeState::request_ui(self, body)
    }
}

/// How long each attempt to reach the session actor, or to observe a view
/// change, blocks before the record state is re-read.
const START_POLL: Duration = Duration::from_secs(5);
/// How long one worker export command may run. A diff of a large checkout over
/// SSH is slow; a target that has stopped answering must not hold the caller's
/// HTTP request open indefinitely.
const EXPORT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// How long a restarted sub-agent's session actor has to connect to the
/// worker the restart already proved ready.
const UNPARK_ATTACH_TIMEOUT: Duration = Duration::from_secs(60);

/// The daemon accepted a parked child's wake, so its record is `Running` with
/// a durable boot intent, but the session was not ready for a prompt within
/// [`UNPARK_ATTACH_TIMEOUT`]. The child is still starting; the wake did not
/// fail. A caller with a delivery deadline keeps waiting for readiness
/// instead of reporting the delivery as failed (#1395).
#[derive(Debug)]
struct WakeNotReady(String);

impl std::fmt::Display for WakeNotReady {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for WakeNotReady {}
/// Clap's exit code for a usage failure, which is what a worker binary too old
/// to know the export subcommands answers.
const CLAP_USAGE_EXIT_CODE: i32 = 2;

/// The status a POSIX shell, SSH, or container exec reports when the command
/// path does not exist: a worker root whose `hel` is absent.
const CONTROL_NOT_FOUND_EXIT_CODE: i32 = 127;

/// The backend the `/api/v1` routes drive sessions through.
pub struct ApiBackend {
    sessions: SessionControl,
    bundle_slots: Arc<tokio::sync::Semaphore>,
    preflight_slots: Arc<tokio::sync::Semaphore>,
    /// The daemon operations the export path needs: session records, and the
    /// checkpoint a bundle export is read from.
    exports: Arc<dyn ExportRuntime>,
    /// Latest background-refreshed quota reports, used to rank configured
    /// session and sub-agent profile choices without exposing credential
    /// aliases to their callers.
    quota_reports: Arc<Mutex<mj_controller::quota::QuotaReportMap>>,
    quota_publications: Option<tokio::sync::watch::Receiver<mj_controller::quota::QuotaReportMap>>,
    quota_refresh: Option<tokio::sync::mpsc::UnboundedSender<crate::daemon::QuotaRefreshIntent>>,
    /// Profiles whose login the credential sync found refused; session and
    /// sub-agent selection refuse them until their login file changes.
    rejected_logins: Arc<Mutex<mj_core::credentials::RejectedLogins>>,
    /// The capabilities user-facing selection and sub-agent operations use.
    /// The catalogue discovers them in the background, so a call waits for a
    /// profile the pass has not published instead of starting another probe.
    profile_catalog: Arc<super::profile_catalog::ProfileCatalog>,
}

struct ChildObservation {
    record: Option<mj_core::state::SessionRecord>,
    parking: bool,
    active: bool,
    view: Option<mj_client::session::ManagedSessionView>,
}

struct NotificationChildSnapshot {
    relation: mj_core::subagent::SubagentRecord,
    progress: ChildProgress,
    record: Option<mj_core::state::SessionRecord>,
    state: String,
    output: Option<String>,
    finished: bool,
    identity: Option<mj_core::subagent::SubagentFinishIdentity>,
}

#[derive(Debug)]
pub(crate) enum QuotaRead {
    Ready(ProfileQuota),
    Pending(QuotaWait),
}

#[derive(Debug)]
pub(crate) struct QuotaWait {
    receiver: tokio::sync::watch::Receiver<mj_controller::quota::QuotaReportMap>,
}

impl QuotaWait {
    pub(crate) async fn wait(mut self) -> Result<()> {
        self.receiver
            .changed()
            .await
            .context("quota publication owner stopped before the profile was checked")
    }
}

impl ApiBackend {
    /// Partition `ids` into eligible profiles and those unavailable because
    /// their login was refused or their required native login file is missing.
    ///
    /// Runs filesystem checks on a blocking thread pool so the Tokio reactor
    /// never blocks on disk I/O.
    async fn filter_eligible_profiles(
        &self,
        ids: Vec<(String, mj_core::config::HarnessKind)>,
        config: &mj_core::config::Config,
    ) -> Result<(
        Vec<(String, mj_core::config::HarnessKind)>,
        Vec<(String, String)>,
    )> {
        let mut eligible = Vec::with_capacity(ids.len());
        let mut unavailable = Vec::new();
        let mut to_check = Vec::new();
        {
            let rejected = self
                .rejected_logins
                .lock()
                .map_err(|_| anyhow!("refused logins lock poisoned"))?;
            for (profile_id, harness) in ids {
                if let Some(reason) = rejected.refusal(&profile_id) {
                    unavailable.push((profile_id, reason));
                    continue;
                }
                let profile = config.enabled_profile(&profile_id).cloned();
                to_check.push((profile_id, harness, profile));
            }
        }
        let checked = mj_core::runtime::spawn_blocking(move || {
            to_check
                .into_iter()
                .map(|(profile_id, harness, profile)| {
                    let missing = profile
                        .as_ref()
                        .and_then(|profile| profile.missing_native_login_reason(&profile_id));
                    (profile_id, harness, missing)
                })
                .collect::<Vec<_>>()
        })
        .await
        .context("profile login check task failed")?;
        for (profile_id, harness, missing) in checked {
            if let Some(reason) = missing {
                unavailable.push((profile_id, reason));
            } else {
                eligible.push((profile_id, harness));
            }
        }
        Ok((eligible, unavailable))
    }

    pub(crate) async fn discover_profile_candidates(
        &self,
        ids: Vec<(String, mj_core::config::HarnessKind)>,
        candidate_kind: &str,
    ) -> Result<crate::server::api::SubagentCandidates> {
        let config = self.profile_catalog.live()?;
        // A profile whose login the provider has refused or whose required
        // login file was removed would fail at launch, so exclude it before
        // discovery or cached capability lookup.
        let (ids, unavailable) = self.filter_eligible_profiles(ids, &config).await?;
        let mut candidates = crate::server::api::SubagentCandidates {
            unavailable,
            ..Default::default()
        };
        // One discovery per profile, so a profile whose harness cannot be
        // discovered drops out on its own instead of failing the whole answer.
        let discoveries = futures::future::join_all(
            ids.iter()
                .map(|(id, _)| self.profile_catalog.capabilities(std::slice::from_ref(id))),
        )
        .await;
        let quota_reports = self
            .quota_reports
            .lock()
            .map_err(|_| anyhow!("profile quota reports lock poisoned"))?;
        let profile_health = mj_controller::profile_health::shared()
            .lock()
            .map_err(|_| anyhow!("profile health lock poisoned"))?;
        let now = std::time::SystemTime::now();
        let now_seconds = i64::try_from(mj_core::clock::epoch_seconds()).unwrap_or(i64::MAX);
        for ((profile_id, harness), discovery) in ids.into_iter().zip(discoveries) {
            match discovery.map(|mut choices| choices.pop()) {
                Ok(Some(choices)) => {
                    let quota = crate::server::api::CandidateQuota::from_publication(
                        quota_reports.get(&profile_id),
                        harness,
                        now_seconds,
                    );
                    let unhealthy = choices
                        .models
                        .iter()
                        .filter_map(|model| {
                            profile_health
                                .unhealthy(&profile_id, &model.value, now)
                                .cloned()
                                .map(|mark| (model.value.clone(), mark))
                        })
                        .collect();
                    let may_consume_credits = config
                        .enabled_profile(&profile_id)
                        .is_some_and(|profile| profile.load_balancing_may_consume_credits);
                    let cache_identity = quota_reports
                        .get(&profile_id)
                        .map(|publication| publication.cache_identity.clone());
                    candidates
                        .offered
                        .push(crate::server::api::SubagentCandidate {
                            profile_id,
                            harness,
                            choices,
                            quota,
                            may_consume_credits,
                            unhealthy,
                            cache_identity,
                        });
                }
                Ok(None) => candidates
                    .unavailable
                    .push((profile_id, "its discovery returned nothing".to_owned())),
                Err(error) => {
                    tracing::warn!(profile_id, %error, "profile left out of model selection");
                    candidates
                        .unavailable
                        .push((profile_id, format!("{error:#}")));
                }
            }
        }
        if candidates.offered.is_empty() && !candidates.unavailable.is_empty() {
            bail!(
                "no {candidate_kind} profile is available: {}",
                format_unavailable_reasons(&candidates.unavailable)
            );
        }
        Ok(candidates)
    }

    pub(crate) async fn reviewer_candidates(
        &self,
    ) -> Result<crate::server::api::SubagentCandidates> {
        self.discover_profile_candidates(self.profile_catalog.candidates()?, "reviewer")
            .await
    }

    pub(crate) async fn start_followup_with_id(
        &self,
        session_id: String,
        followup: StartFollowup,
        group_id: String,
    ) -> Result<()> {
        if followup == StartFollowup::default() {
            return Ok(());
        }
        Arc::clone(&self.exports)
            .queue_startup(session_id, group_id, followup)
            .await
    }

    pub fn new(
        sessions: SessionControl,
        _session_states: SessionStateSource,
        exports: Arc<dyn ExportRuntime>,
    ) -> Self {
        Self {
            sessions,
            bundle_slots: Arc::new(tokio::sync::Semaphore::new(
                super::MAX_CONCURRENT_BUNDLE_CREATIONS,
            )),
            preflight_slots: Arc::new(tokio::sync::Semaphore::new(
                super::MAX_CONCURRENT_PREFLIGHTS,
            )),
            exports,
            quota_reports: Arc::new(Mutex::new(BTreeMap::new())),
            quota_publications: None,
            quota_refresh: None,
            rejected_logins: Arc::default(),
            // Nothing is adopted until the daemon hands its configuration
            // over, so a backend built without one — every test that does not
            // care about profiles — reports that `list_profiles` has nothing
            // to answer from instead of discovering on the call.
            profile_catalog: super::profile_catalog::ProfileCatalog::new(
                tokio_util::sync::CancellationToken::new(),
            ),
        }
    }

    pub(crate) fn with_quota_reports(
        mut self,
        quota_reports: Arc<Mutex<mj_controller::quota::QuotaReportMap>>,
    ) -> Self {
        self.quota_reports = quota_reports;
        self
    }

    pub(crate) fn with_quota_publications(
        mut self,
        publications: tokio::sync::watch::Receiver<mj_controller::quota::QuotaReportMap>,
    ) -> Self {
        self.quota_publications = Some(publications);
        self
    }

    pub(crate) fn with_quota_refresh(
        mut self,
        refresh: tokio::sync::mpsc::UnboundedSender<crate::daemon::QuotaRefreshIntent>,
    ) -> Self {
        self.quota_refresh = Some(refresh);
        self
    }

    /// Full poller-owned report, including model-scoped allowances omitted
    /// from user-facing quota projections.
    pub(crate) fn quota_report(
        &self,
        profile_id: &str,
        wanted_identity: &str,
    ) -> Result<QuotaRead> {
        let publications = self
            .quota_publications
            .as_ref()
            .ok_or_else(|| anyhow!("quota publication source is unavailable"))?;
        let mut receiver = publications.clone();
        // Inspect one versioned owner snapshot. A later publication wakes the
        // returned waiter even if it arrived before the wait started.
        let current = receiver.borrow_and_update().clone();
        let config = self.profile_catalog.live()?;
        if config.enabled_profile(profile_id).is_none() {
            bail!("quota profile {profile_id:?} is missing or disabled");
        }
        match current.get(profile_id) {
            Some(publication) if publication.cache_identity == wanted_identity => {
                match publication.effective_report(profile_id) {
                    Some(report) => Ok(QuotaRead::Ready(report)),
                    None => Ok(QuotaRead::Pending(QuotaWait { receiver })),
                }
            }
            _ => {
                self.quota_refresh
                    .as_ref()
                    .ok_or_else(|| anyhow!("quota source reconciliation is unavailable"))?
                    .send(crate::daemon::QuotaRefreshIntent::ReconcileSources)
                    .map_err(|_| anyhow!("quota owner stopped before source reconciliation"))?;
                Ok(QuotaRead::Pending(QuotaWait { receiver }))
            }
        }
    }

    pub fn with_rejected_logins(
        mut self,
        rejected_logins: Arc<Mutex<mj_core::credentials::RejectedLogins>>,
    ) -> Self {
        self.rejected_logins = rejected_logins;
        self
    }

    pub fn with_profile_catalog(
        mut self,
        profile_catalog: Arc<super::profile_catalog::ProfileCatalog>,
    ) -> Self {
        self.profile_catalog = profile_catalog;
        self
    }

    pub(crate) fn profile_catalog(&self) -> &Arc<super::profile_catalog::ProfileCatalog> {
        &self.profile_catalog
    }

    /// Recover the immutable effect selection before doing any external work.
    pub(crate) async fn execute_subagent_tool_durable(
        self: &Arc<Self>,
        parent: String,
        request: mj_core::subagent::SubagentToolRequest,
    ) -> Result<crate::daemon::delegation::DelegationResult> {
        let stored = blocking("load delegation effect", {
            let parent = parent.clone();
            let id = request.request_id.clone();
            move || mj_controller::database::load_delegation(&parent, &id)
        })
        .await?;
        if stored.is_none()
            && matches!(
                request.action,
                mj_core::subagent::SubagentToolAction::ListAgents
            )
        {
            let result = self
                .execute_subagent_tool_prepared(parent, request, None, None)
                .await;
            return Ok(crate::daemon::delegation::DelegationResult {
                result,
                persisted: false,
            });
        }
        let prepared = if let Some((prepared, result)) = stored {
            if let Some(result) = result {
                if matches!(
                    prepared.request.action,
                    mj_core::subagent::SubagentToolAction::LegacyWaitAgents
                        | mj_core::subagent::SubagentToolAction::LegacyAwaitChildren
                ) {
                    return blocking("retire legacy wait", {
                        let parent = parent.clone();
                        let id = request.request_id.clone();
                        move || mj_controller::database::retire_legacy_wait(parent, id)
                    })
                    .await?
                    .context("legacy wait result disappeared before conversion")
                    .map(Into::into);
                }
                return Ok(result.into());
            }
            prepared
        } else {
            let mut request = request;
            self.canonicalize_subagent_target(&parent, &mut request)
                .await?;
            let turn_target = if matches!(
                request.action,
                mj_core::subagent::SubagentToolAction::Handback { .. }
            ) {
                request.originating_command_id.clone()
            } else {
                None
            };
            blocking("prepare delegation effect", {
                let parent = parent.clone();
                move || {
                    mj_controller::database::prepare_delegation(
                        parent,
                        mj_controller::database::PreparedDelegation {
                            request,
                            turn_target,
                            spawn: None,
                        },
                    )
                }
            })
            .await?
        };
        // A child's handback is final only once the review host can say
        // whether the child's review is open. The state is read once, here,
        // and the handback is decided from that one answer. While the host
        // restores reviews or the daemon stops, the request stays unanswered:
        // this error is a task failure, which the dispatcher retries with
        // backoff, and no refusal is stored for a child that may never read it.
        let handback_review = self.handback_review(&parent, &prepared.request).await?;
        if handback_review == Some(crate::daemon::ChildReview::Unknown) {
            bail!(
                "the review host cannot yet say whether this child's review is open; the handback will be retried"
            );
        }
        blocking("mark delegation delivering", {
            let parent = parent.clone();
            let id = prepared.request.request_id.clone();
            move || mj_controller::database::delegation_delivering(parent, id)
        })
        .await?;
        let result = self
            .execute_subagent_tool_prepared(
                parent.clone(),
                prepared.request.clone(),
                Some(&prepared),
                handback_review,
            )
            .await;
        let stored_result = mj_controller::database::StoredDelegationResult {
            result,
            reported_finishes: Vec::new(),
        };
        blocking("persist delegation result", {
            let result = stored_result.clone();
            move || mj_controller::database::record_stored_delegation_result(parent, result)
        })
        .await?;
        Ok(stored_result.into())
    }

    /// Store a resolved target in durable MCP requests so a replay does not
    /// reinterpret a prefix after the session set changes.
    async fn canonicalize_subagent_target(
        &self,
        parent_session_id: &str,
        request: &mut mj_core::subagent::SubagentToolRequest,
    ) -> Result<()> {
        let Some(requested_id) = (match &request.action {
            mj_core::subagent::SubagentToolAction::SendMessage {
                child_session_id, ..
            } => Some(child_session_id.clone()),
            mj_core::subagent::SubagentToolAction::CloseAgent { child_session_id } => {
                Some(child_session_id.clone())
            }
            _ => None,
        }) else {
            return Ok(());
        };
        let parent_session_id = parent_session_id.to_owned();
        let child_management = matches!(
            request.action,
            mj_core::subagent::SubagentToolAction::CloseAgent { .. }
        );
        let resolved = blocking("resolve durable sub-agent target", move || {
            let state = Controller::load()?.state;
            subagent_input::resolve_session_target(
                &state,
                child_management.then_some(parent_session_id.as_str()),
                &requested_id,
            )
        })
        .await;
        match resolved {
            Ok(resolved_id) => {
                match &mut request.action {
                    mj_core::subagent::SubagentToolAction::SendMessage {
                        child_session_id, ..
                    }
                    | mj_core::subagent::SubagentToolAction::CloseAgent { child_session_id } => {
                        *child_session_id = resolved_id;
                    }
                    _ => unreachable!("only target-bearing actions are canonicalized"),
                }
                Ok(())
            }
            // Keep refused requests in their original form so the normal
            // execution path can persist and return the same refusal.
            Err(error) if mj_core::refusal::Refusal::of(&error).is_some() => Ok(()),
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    pub async fn execute_subagent_tool(
        self: &Arc<Self>,
        parent_session_id: String,
        request: mj_core::subagent::SubagentToolRequest,
    ) -> mj_core::subagent::SubagentToolResult {
        let handback_review = self
            .handback_review(&parent_session_id, &request)
            .await
            .ok()
            .flatten();
        self.execute_subagent_tool_prepared(parent_session_id, request, None, handback_review)
            .await
    }

    /// Whether the child's worker still queues a handback the daemon has not
    /// executed, by the rule child completion uses. A child that is not live
    /// has published no queue, so nothing of its is pending here.
    async fn child_handback_pending(&self, child_id: &str) -> Result<bool> {
        let Ok(handle) = self.sessions.session(child_id.to_owned()).await else {
            return Ok(false);
        };
        let Some(observation) =
            mj_controller::session_manager::DelegationObservation::from_view(&handle.view())
        else {
            return Ok(false);
        };
        crate::daemon::delegation::handback_pending(child_id, &observation).await
    }

    /// For an ordinary worker child's handback, whether its review is open,
    /// asked once so the decision rests on one answer. `None` for any other
    /// action and for reviewers and oracles, which have no review of their
    /// own and so never wait for the review host.
    async fn handback_review(
        &self,
        requester: &str,
        request: &mj_core::subagent::SubagentToolRequest,
    ) -> Result<Option<crate::daemon::ChildReview>> {
        if !matches!(
            request.action,
            mj_core::subagent::SubagentToolAction::Handback { .. }
        ) {
            return Ok(None);
        }
        let worker = blocking("load sub-agent kind", {
            let requester = requester.to_owned();
            move || mj_controller::database::load_subagent(&requester)
        })
        .await?
        .is_some_and(|record| record.kind == mj_core::subagent::SubagentKind::Worker);
        if !worker {
            return Ok(None);
        }
        Ok(Some(self.exports.child_review(requester.to_owned()).await))
    }

    async fn execute_subagent_tool_prepared(
        self: &Arc<Self>,
        parent_session_id: String,
        request: mj_core::subagent::SubagentToolRequest,
        prepared: Option<&mj_controller::database::PreparedDelegation>,
        handback_review: Option<crate::daemon::ChildReview>,
    ) -> mj_core::subagent::SubagentToolResult {
        let outcome = self
            .execute_subagent_tool_inner(&parent_session_id, &request, prepared, handback_review)
            .await;
        let child_id = match &request.action {
            mj_core::subagent::SubagentToolAction::SendInput {
                child_session_id, ..
            } => Some((child_session_id, "input")),
            mj_core::subagent::SubagentToolAction::SendMessage {
                child_session_id, ..
            } => Some((child_session_id, "message")),
            _ => None,
        };
        if let Some((child_session_id, kind)) = child_id {
            let (mut value, is_error) = match outcome {
                Ok(value) => (value, false),
                Err(error) => {
                    let via = error
                        .downcast_ref::<subagent_input::InputDeliveryFailure>()
                        .map_or("turn", |failure| failure.via);
                    (
                        serde_json::json!({
                            "status":"failed",
                            "via":via,
                            "error":format!("{error:#}")
                        }),
                        true,
                    )
                }
            };
            let result_session_id = if kind == "message" && !is_error {
                value
                    .get("child_session_id")
                    .or_else(|| value.get("session_id"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(child_session_id)
                    .to_owned()
            } else {
                child_session_id.clone()
            };
            value["child_session_id"] = result_session_id.into();
            value["request_id"] = request.request_id.clone().into();
            value["created_at_ms"] = request.created_at_ms.into();
            value["kind"] = kind.into();
            return mj_core::subagent::SubagentToolResult {
                request_id: request.request_id,
                completed_at_ms: mj_core::clock::epoch_millis(),
                is_error,
                message: value.to_string(),
            };
        }
        let (is_error, message) = match outcome {
            Ok(value) => (
                false,
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
            ),
            Err(error) => (true, format!("{error:#}")),
        };
        mj_core::subagent::SubagentToolResult {
            request_id: request.request_id,
            completed_at_ms: mj_core::clock::epoch_millis(),
            is_error,
            message,
        }
    }

    async fn execute_subagent_tool_inner(
        self: &Arc<Self>,
        parent_session_id: &str,
        request: &mj_core::subagent::SubagentToolRequest,
        prepared: Option<&mj_controller::database::PreparedDelegation>,
        handback_review: Option<crate::daemon::ChildReview>,
    ) -> Result<serde_json::Value> {
        use mj_core::subagent::SubagentToolAction;
        let request_created_at_ms = request.created_at_ms;
        match &request.action {
            SubagentToolAction::Fetch {
                session_id,
                path,
                ls,
            } => {
                ensure!(
                    mj_controller::database::load_subagent(session_id)?.is_none_or(|relation| {
                        relation.kind != mj_core::subagent::SubagentKind::Oracle
                            || relation.parent_session_id != parent_session_id
                    }),
                    "session {session_id} does not belong to parent {parent_session_id}"
                );
                let fetch_error = |error| match error {
                    ExportError::Refused(reason) => anyhow!(reason),
                    ExportError::Failed(error) => error,
                };
                ensure!(
                    !path.as_os_str().is_empty(),
                    "path must name a file or directory on the session's target"
                );
                let mode = if *ls {
                    FetchMode::Listing
                } else {
                    FetchMode::Content
                };
                if !*ls {
                    // Both sessions on one remote host: the payload stays there.
                    self.require_live_target(session_id).map_err(fetch_error)?;
                    self.require_live_target(parent_session_id)
                        .map_err(fetch_error)?;
                    if let Some(output) = self
                        .exports
                        .try_fetch_same_host(
                            parent_session_id.to_owned(),
                            session_id.clone(),
                            path.clone(),
                        )
                        .await
                        .map_err(fetch_error)?
                    {
                        let saved: mj_checkpoint::archive::SavedFetch =
                            serde_json::from_slice(&output).context("decode the saved fetch")?;
                        return Ok(serde_json::to_value(saved)?);
                    }
                }
                let fetched = self
                    .read_file(session_id.clone(), path.clone(), mode)
                    .await
                    .map_err(fetch_error)?;
                if *ls {
                    let listing: mj_checkpoint::archive::SessionListing =
                        blocking("decode fetched listing", move || {
                            serde_json::from_reader(fetched.reader()).context("decode the listing")
                        })
                        .await?;
                    return Ok(serde_json::to_value(listing)?);
                }
                self.require_live_target(parent_session_id)
                    .map_err(fetch_error)?;
                let output = fetch_transfer::store(
                    parent_session_id.to_owned(),
                    session_id.clone(),
                    fetched,
                )
                .await
                .map_err(fetch_error)?;
                let saved: mj_checkpoint::archive::SavedFetch =
                    serde_json::from_slice(&output).context("decode the saved fetch")?;
                Ok(serde_json::to_value(saved)?)
            }
            SubagentToolAction::RequestReview { paths } => {
                self.exports
                    .request_review(parent_session_id.to_owned(), paths.clone())
                    .await
            }
            SubagentToolAction::AskOracle { message } => {
                self.ask_oracle(parent_session_id, request, prepared, message)
                    .await
            }
            SubagentToolAction::OverrideReview { overrides, path } => {
                self.exports
                    .override_review(
                        parent_session_id.to_owned(),
                        path.clone(),
                        overrides.clone(),
                    )
                    .await
            }
            SubagentToolAction::SetReviewBaseline { commit, path } => {
                self.exports
                    .set_review_baseline(parent_session_id.to_owned(), commit.clone(), path.clone())
                    .await
            }
            SubagentToolAction::ListProfiles => {
                anyhow::bail!("list_profiles is no longer available")
            }
            SubagentToolAction::Spawn {
                no_github_credentials,
                task_name,
                instructions,
                task_type,
                working_directory,
                context,
                files,
            } => {
                let (selection, reservation) =
                    if let Some(selection) = prepared.and_then(|p| p.spawn.clone()) {
                        (selection, None)
                    } else {
                        let parent = self
                            .exports
                            .session_record(parent_session_id)
                            .context("parent session disappeared")?;
                        let requested_orchestrator =
                            *task_type == mj_core::subagent::SubagentTaskType::Orchestrator;
                        let parent_subagent = blocking("load parent subagent record", {
                            let parent_id = parent_session_id.to_owned();
                            move || mj_controller::database::load_subagent(&parent_id)
                        })
                        .await?;
                        mj_core::subagent::validate_subagent_parent(
                            parent_subagent.as_ref(),
                            false,
                            mj_core::subagent::SubagentKind::Worker,
                            requested_orchestrator,
                        )?;
                        if requested_orchestrator {
                            ensure!(
                                parent.subagents.as_ref().is_some_and(
                                    mj_core::subagent::SubagentPolicy::orchestrators_enabled
                                ),
                                "orchestrators are not enabled for this session"
                            );
                        }
                        let backend: Arc<dyn crate::server::api::SubagentBackend> = self.clone();
                        let selection = crate::server::api::resolve_subagent_policy_selection(
                            &backend,
                            &parent.last_profile,
                            &parent.subagents.clone().unwrap_or_default(),
                            *task_type,
                        )
                        .await
                        .map_err(|failure| anyhow::anyhow!(failure.message))?;
                        // Hold the atomic placement reservation until registration succeeds.
                        let reservation = selection.reservation.clone();
                        // This wire queue can contain requests accepted by older workers.
                        let ranges = files
                            .iter()
                            .flat_map(|entry| {
                                let file = entry.file.clone();
                                entry.ranges.iter().map(move |range| {
                                    crate::server::api::LegacySubagentSourceRange {
                                        file: file.clone(),
                                        start: range.start,
                                        end: range.end,
                                    }
                                })
                            })
                            .collect::<Vec<_>>();
                        let prompt = crate::server::api::build_legacy_subagent_prompt(
                            &backend,
                            parent_session_id,
                            instructions,
                            context.as_deref(),
                            &ranges,
                        )
                        .await
                        .map_err(|failure| anyhow::anyhow!(failure.message))?;
                        let selected = mj_controller::database::PreparedSpawn {
                            profile_id: selection.profile_id,
                            model: selection.model,
                            effort: selection.effort,
                            fast_mode: selection.fast_mode,
                            prompt,
                        };
                        let selected = if prepared.is_some() {
                            blocking("persist selected spawn", {
                                let parent = parent_session_id.to_owned();
                                let request = request.request_id.clone();
                                move || {
                                    mj_controller::database::prepare_delegation_spawn(
                                        parent, request, selected,
                                    )
                                }
                            })
                            .await?
                        } else {
                            selected
                        };
                        (selected, Some(reservation))
                    };
                let relation = self
                    .start_subagent(mj_controller::controller::RegisterSubagentRequest {
                        no_github_credentials: *no_github_credentials,
                        kind: mj_core::subagent::SubagentKind::Worker,
                        orchestrator: *task_type
                            == mj_core::subagent::SubagentTaskType::Orchestrator,
                        reviewer_launch: None,
                        parent_session_id: parent_session_id.to_owned(),
                        task_name: task_name.clone(),
                        profile_id: selection.profile_id,
                        model: Some(selection.model.clone()),
                        effort: selection.effort.clone(),
                        fast_mode: selection.fast_mode,
                        working_directory: working_directory.clone(),
                        initial_prompt: selection.prompt,
                        request_key: request.request_id.clone(),
                        report_root: None,
                        category: task_type.id().to_owned(),
                    })
                    .await?;
                if let Some(reservation) = reservation {
                    reservation.commit(Some(relation.child_session_id.clone()));
                }
                let report_dir = blocking("load sub-agent report directory", {
                    let child_id = relation.child_session_id.clone();
                    move || mj_controller::database::load_subagent_report(&child_id)
                })
                .await?
                .report_dir;
                let name = live_child_name(
                    self.exports
                        .session_record(&relation.child_session_id)
                        .as_ref(),
                    &relation.task_name,
                );
                Ok(serde_json::json!({
                    "child_session_id":relation.child_session_id,
                    "task_name":relation.task_name,
                    "name":name,
                    "profile_id":relation.profile_id,
                    "model":relation.model,
                    "effort":relation.effort,
                    "report_dir":report_dir,
                }))
            }
            SubagentToolAction::SpawnOrchestrator { .. } => {
                let relation = Arc::clone(&self.exports)
                    .spawn_orchestrator(
                        parent_session_id.to_owned(),
                        request.clone(),
                        prepared.cloned(),
                    )
                    .await?;
                Ok(serde_json::json!({
                    "child_session_id": relation.child_session_id,
                    "task_name": relation.task_name,
                    "name": relation.task_name,
                    "profile_id": relation.profile_id,
                    "model": relation.model,
                    "effort": relation.effort,
                }))
            }
            SubagentToolAction::ListTargets => {
                let projection = self
                    .exports
                    .runtime_projection()?
                    .context("target listing is unavailable")?;
                list_targets(
                    &projection,
                    parent_session_id,
                    mj_core::clock::epoch_seconds(),
                )
            }
            SubagentToolAction::ListAgents => {
                // This request was read from the parent's queue, so the queue
                // is known here unless the parent's actor went away since; the
                // listing then shows the mailbox's part, and decides nothing.
                let snapshot = match self.subagent_queue_snapshot(parent_session_id).await {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        tracing::debug!(parent_session_id, %error, "parent queue unavailable for list_agents");
                        None
                    }
                };
                let inputs = self
                    .input_progress_with_mailbox(parent_session_id, snapshot.as_ref())
                    .await?;
                let mut pending_spawns =
                    pending_spawn_entries(parent_session_id, snapshot.as_ref()).await?;
                let details = blocking("load children and their selections", {
                    let parent = parent_session_id.to_owned();
                    move || mj_controller::database::list_subagent_details(&parent)
                })
                .await?
                .into_iter()
                .filter(|detail| detail.relation.kind == mj_core::subagent::SubagentKind::Worker)
                .collect::<Vec<_>>();
                for detail in &details {
                    pending_spawns.remove(&detail.relation.request_key);
                }
                let child_ids = details
                    .iter()
                    .map(|detail| detail.relation.child_session_id.clone())
                    .collect::<Vec<_>>();
                let (observed, starts) = self.child_snapshots(child_ids).await?;
                let summaries = observed
                    .into_iter()
                    .map(|(id, summary, progress)| (id, (summary, progress)))
                    .collect::<BTreeMap<_, _>>();
                let now_ms = u64::try_from(mj_core::clock::epoch_millis()).unwrap_or(0);
                let projection = self.exports.runtime_projection()?;
                let active_children = projection
                    .as_ref()
                    .map_or_else(BTreeMap::new, live_direct_children);
                let agents = details.into_iter()
                    .map(|detail| {
                        let relation = &detail.relation;
                        let record = projection.as_ref().and_then(|projection| {
                            projection.records.get(&relation.child_session_id).cloned()
                        }).or_else(|| self.exports.session_record(&relation.child_session_id));
                        // This listing reports state; the mailbox carries reports.
                        let unknown = ChildProgress {
                            finished_span: None,
                            report_answer: None,
                            unanswered_messages: None,
                            observed_handback: None,
                            report_failure_message: None,
                            last_completed_ordinal: None,
                            report: ReportState::Fallback,
                            awaited_ordinal: None,
                            answer: None,
                            state: ChildState::WaitingForWorker,
                            failure_message: None,
                            wake_error: None,
                            report_dir: None,
                        };
                        let (summary, progress) = summaries
                            .get(&relation.child_session_id)
                            .map_or((None, &unknown), |(summary, progress)| {
                                (summary.as_ref(), progress)
                            });
                        let (state, output, _) = inputs.status(
                            &relation.child_session_id,
                            record.as_ref(),
                            subagent_status(
                                record.as_ref(),
                                summary,
                                starts.get(&relation.child_session_id),
                                None,
                                self.exports.close_is_requested(&relation.child_session_id),
                                progress,
                            ),
                        );
                        let name = live_child_name(record.as_ref(), &relation.task_name);
                        let mut entry = serde_json::json!({
                            "child_session_id":relation.child_session_id,
                            "task_name":relation.task_name,
                            "spawn_request_id":relation.request_key,
                            "name":name,
                            "role":listed_agent_role(relation, detail.category.as_deref()),
                            "target":record.as_ref().map(|r| &r.target_template_id),
                            "repos":agent_repo_urls(record.as_ref(), projection.as_ref()),
                            "profile_id":record.as_ref().map_or(&relation.profile_id, |record| &record.last_profile),
                            "state":state,
                            "active_children": active_children.get(relation.child_session_id.as_str()).copied().unwrap_or(0),
                        });
                        if relation.orchestrator
                            && projection.as_ref().is_some_and(|projection| {
                                projection.environment_owners.get(&relation.child_session_id)
                                    == Some(&relation.child_session_id)
                            })
                            && let Some(report_dir) = &progress.report_dir
                        {
                            entry["report_dir"] = report_dir.clone().into();
                        }
                        let selection = live_agent_selection(projection.as_ref(), &detail, record.as_ref().map(|record| record.last_profile.as_str()));
                        entry["model"] = serde_json::json!(selection.model);
                        entry["effort"] = serde_json::json!(selection.effort);
                        inputs.annotate(&relation.child_session_id, &mut entry);
                        if state == "error"
                            && let Some(cause) = output
                        {
                            entry["error"] = cause.into();
                            entry["next_action"] = mj_core::subagent::FAILED_CHILD_CLOSE_GUIDANCE.into();
                        }
                        if record.as_ref().is_some_and(|r| r.state == SessionState::Provisioning)
                            && let Some(projection) = &projection
                            && let Some(lifecycle) = projection.metadata.lifecycles.iter().find(|op| {
                                op.session_id == relation.child_session_id && !op.active_stages.is_empty()
                            })
                        {
                            entry["provisioning_stages"] = serde_json::json!(lifecycle.active_stages
                                .iter().map(|(stage, _)| stage.label()).collect::<Vec<_>>());
                        }
                        mark_parked(&mut entry, record.as_ref());
                        mark_promptable(&mut entry, &state, progress, record.as_ref());
                        if let ReportState::Held {
                            since_ms,
                            give_up_at_ms,
                            waiting_on,
                            ..
                        } = &progress.report
                        {
                            entry["waiting_on"] = serde_json::json!({
                                "processes": waiting_on,
                                "since_ms": since_ms,
                                "give_up_at_ms": give_up_at_ms,
                            });
                        }
                        if let Some(facts) = progress
                            .answer
                            .as_ref()
                            .and_then(|turn| turn.work_facts.as_ref())
                        {
                            entry["facts"] = work_facts_entry(facts, now_ms);
                        }
                        entry
                    })
                    .collect::<Vec<_>>();
                let mut listing = serde_json::json!({"agents":agents});
                if !inputs.queue_known() {
                    listing["queue_status"] = "unavailable".into();
                }
                if !pending_spawns.is_empty() {
                    listing["pending_spawns"] =
                        serde_json::json!(pending_spawns.into_values().collect::<Vec<_>>());
                }
                Ok(listing)
            }
            SubagentToolAction::SendInput {
                child_session_id,
                message,
            } => {
                self.require_owned_child(parent_session_id, child_session_id)
                    .await?;
                let delivery = self
                    .deliver_to_own_child(
                        child_session_id.clone(),
                        message.clone(),
                        request.request_id.clone(),
                        request_created_at_ms,
                    )
                    .await?;
                if let Some(ordinal) = delivery.turn_id {
                    blocking("record legacy sub-agent prompt", {
                        let child = delivery.session_id.clone();
                        move || mj_controller::database::record_subagent_prompt(&child, ordinal)
                    })
                    .await?;
                }
                subagent_message_result(delivery)
            }
            SubagentToolAction::SendMessage {
                child_session_id,
                message,
            } => {
                let delivery = self
                    .deliver_session_message(
                        parent_session_id.to_owned(),
                        child_session_id.clone(),
                        message.clone(),
                        request.request_id.clone(),
                        request_created_at_ms,
                    )
                    .await?;
                subagent_message_result(delivery)
            }
            SubagentToolAction::LegacyWaitAgents | SubagentToolAction::LegacyAwaitChildren => {
                Ok(serde_json::json!({
                    "status": "reports_arrive_as_messages",
                    "next_action": mj_core::subagent::REPORT_NOTIFICATION_NEXT_ACTION
                }))
            }
            SubagentToolAction::LegacyInterruptAgent { .. } => {
                bail!("the legacy interrupt action is no longer supported; use send_message")
            }
            SubagentToolAction::CloseAgent { child_session_id } => {
                let child_session_id = self
                    .resolve_owned_child(parent_session_id, child_session_id)
                    .await?;
                let owns_environment = blocking("read child environment ownership", {
                    let child_session_id = child_session_id.clone();
                    move || mj_controller::database::session_owns_environment(&child_session_id)
                })
                .await?;
                if owns_environment {
                    return Arc::clone(&self.exports)
                        .close_orchestrator(child_session_id)
                        .await;
                }
                if prepared.is_some() {
                    Arc::clone(&self.exports)
                        .close_subagent_request(
                            child_session_id.clone(),
                            parent_session_id.to_owned(),
                            request.request_id.clone(),
                        )
                        .await?;
                } else {
                    Arc::clone(&self.exports)
                        .close_subagent(child_session_id.clone())
                        .await?;
                }
                Ok(serde_json::json!({"child_session_id":child_session_id,"closed":true}))
            }
            // The requester is the child itself: only a child's worker serves
            // this action.
            SubagentToolAction::Handback { message } => {
                let child_id = parent_session_id.to_owned();
                ensure!(
                    !message.trim().is_empty(),
                    "a report cannot be empty; call handback with your full report"
                );
                let record = blocking("load sub-agent record", {
                    let child_id = child_id.clone();
                    move || mj_controller::database::load_subagent(&child_id)
                })
                .await?
                .filter(|record| record.handback_tool)
                .context("handback is only for a Mjolnir sub-agent that was given the tool")?;
                // Refusing here means no report exists to release before the
                // child has read and addressed its verdict.
                if record.kind == mj_core::subagent::SubagentKind::Worker {
                    match handback_review.context("a handback is executed with its review state")? {
                        crate::daemon::ChildReview::Open => bail!(
                            "your review has no verdict yet; wait for it, address it, then hand back"
                        ),
                        crate::daemon::ChildReview::Unknown => bail!(
                            "Mjolnir is restarting and cannot check your review yet; call handback again in a moment"
                        ),
                        crate::daemon::ChildReview::Closed => {}
                    }
                    if record.orchestrator {
                        let active = self.orchestrator_active_children(&child_id).await?;
                        if !active.is_empty() {
                            let list = active
                                .iter()
                                .map(|child| {
                                    format!("{} ({})", child.task_name, child.child_session_id)
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            bail!(
                                "wait for or close your running sub-agents before calling handback: {list}"
                            );
                        }
                    }
                }
                let maximum = record.kind.max_handback_chars();
                let characters = message.chars().count();
                ensure!(
                    characters <= maximum,
                    "a report can be at most {} characters and this one has {}. Write the details \
                     to files in the report directory named in your first prompt, then call \
                     handback again with a short report that lists their paths",
                    maximum,
                    characters
                );
                let command_id = if let Some(prepared) = prepared {
                    prepared
                        .turn_target
                        .clone()
                        .context("no turn was running when this report was prepared")?
                } else {
                    // The live view knows the running turn first; the store
                    // catches up a moment later.
                    let live_turn = self
                        .session_handle(child_id.clone())
                        .await?
                        .and_then(|handle| handle.view().snapshot)
                        .and_then(|snapshot| snapshot.materialized.active_turn);
                    let active_turn = match live_turn {
                        Some(turn) => Some(turn),
                        None => blocking("load child turn", {
                            let child_id = child_id.clone();
                            move || {
                                mj_controller::database::load_materialized_turn_outcome(&child_id)
                            }
                        })
                        .await?
                        .and_then(|(_, active, _)| active),
                    };
                    let turn = active_turn.context(
                    "no turn is running, so there is nothing to report on; hand back your report \
                     during the turn that did the work",
                )?;
                    turn.command_id
                };
                let recorded = blocking("record sub-agent report", {
                    let child_id = child_id.clone();
                    let handback = mj_core::subagent::SubagentHandback {
                        command_id,
                        message: message.clone(),
                        recorded_at_ms: mj_core::clock::epoch_millis(),
                    };
                    move || mj_controller::database::record_subagent_handback(&child_id, &handback)
                })
                .await?;
                Ok(if recorded {
                    serde_json::json!({
                        "delivered": true,
                        "message": "Report delivered to the session that started you.",
                    })
                } else {
                    serde_json::json!({
                        "delivered": false,
                        "message": "Nothing was sent: your report for this turn was already delivered. Stop now.",
                    })
                })
            }
        }
    }

    /// Start a parked child's worker again and wait until its session can
    /// take a prompt. Returns whether there was a park to wait for or undo; a
    /// running child with no park in progress needs nothing and returns
    /// false. A failed wake reports the outcome of the lifecycle operation.
    async fn unpark_child(&self, child_id: &str) -> Result<bool> {
        let parked = self
            .exports
            .session_record(child_id)
            .is_some_and(|record| record.state == SessionState::Parked)
            || self.exports.subagent_park_running(child_id);
        if !parked {
            return Ok(false);
        }
        let child = child_id.to_owned();
        blocking("admit parked child restart", move || {
            let controller = Controller::load()?;
            let relation = controller
                .state
                .subagents
                .get(&child)
                .context("child relationship no longer exists")?;
            controller.ensure_subagent_slot_available(&relation.parent_session_id, Some(&child))
        })
        .await?;
        Arc::clone(&self.exports)
            .unpark_subagent(child_id.to_owned())
            .await
            .map_err(|error| {
                let detail = match self.exports.session_record(child_id).map(|record| record.state) {
                    Some(SessionState::Parked) => {
                        "could not start the parked sub-agent again; it is still parked, so you can retry"
                    }
                    Some(SessionState::Running) => {
                        "could not finish starting the sub-agent; an accepted wake remains recoverable"
                    }
                    _ => "could not start the parked sub-agent again",
                };
                error.context(detail)
            })?;
        if self
            .exports
            .session_record(child_id)
            .is_none_or(|record| record.state != SessionState::Running)
        {
            return Ok(false);
        }
        let deadline = tokio::time::Instant::now() + UNPARK_ATTACH_TIMEOUT;
        let mut handle = self
            .sessions
            .wait_for_session(child_id, UNPARK_ATTACH_TIMEOUT)
            .await
            .map_err(|error| {
                anyhow::Error::new(WakeNotReady(format!(
                    "the restarted sub-agent did not reattach: {error:#}"
                )))
            })?;
        loop {
            let view = handle.view();
            if view.connected
                && view
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.operational.native_session_is_ready())
            {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow::Error::new(WakeNotReady(format!(
                    "the restarted sub-agent was not ready for a prompt within {} seconds",
                    UNPARK_ATTACH_TIMEOUT.as_secs()
                ))));
            }
            let _ = tokio::time::timeout(START_POLL, handle.changed()).await;
        }
    }

    /// What the store and the start tracker say about each of `ids`.
    async fn child_snapshots(
        &self,
        ids: Vec<String>,
    ) -> Result<(
        Vec<(
            String,
            Option<mj_core::state::MaterializedSessionSummary>,
            ChildProgress,
        )>,
        std::collections::BTreeMap<String, StartStatus>,
    )> {
        // The worker owns whether the processes a finished turn left are still
        // alive, so each child's liveness is read before its report is judged.
        let mut observations = Vec::with_capacity(ids.len());
        for id in &ids {
            let (record, parking) = self.exports.child_park_snapshot(id);
            let active = record
                .as_ref()
                .is_some_and(|r| record_may_be_running(r.state));
            let view = if active {
                self.session_handle(id.clone())
                    .await?
                    .map(|handle| handle.view())
            } else {
                None
            };
            observations.push(ChildObservation {
                record,
                parking,
                active,
                view,
            });
        }
        let live = ids
            .iter()
            .zip(&observations)
            .map(|(id, observation)| {
                let liveness = if observation.active {
                    liveness_of(observation.view.as_ref())
                } else {
                    mj_core::subagent::Liveness::Known(None)
                };
                (id.clone(), liveness)
            })
            .collect::<BTreeMap<_, _>>();
        if let Some(committed) = mj_controller::database::committed_state()? {
            let mut starts = BTreeMap::new();
            let summaries = ids
                .into_iter()
                .map(|id| {
                    if let Some(status) = startup_context(committed.startup_groups.get(&id))?.1 {
                        starts.insert(id.clone(), status);
                    }
                    let summary = committed.turns.get(&id).map(|turn| {
                        mj_core::state::MaterializedSessionSummary {
                            session_id: id.clone(),
                            applied_event_ordinal: 0,
                            last_activity_at_ms: None,
                            execution: turn.state.0,
                            session_title: None,
                            last_agent_message: None,
                            last_user_message: None,
                            last_agent_message_follows_last_user: false,
                            agent_message_latest_content_ordinals: Vec::new(),
                            interruption_event_ordinals: Vec::new(),
                        }
                    });
                    let progress = child_progress(&committed, &id, &live[&id]);
                    Ok((id, summary, progress))
                })
                .collect::<Result<Vec<_>>>()?;
            let mut summaries = summaries;
            load_unanswered_wake_reports(&mut summaries).await?;
            self.resolve_child_states(&mut summaries, observations)
                .await?;
            return Ok((summaries, starts));
        }
        // An offline backend has no writer publication to observe.
        let mut summaries = mj_core::runtime::spawn_blocking(move || {
            ids.into_iter()
                .map(|id| {
                    let summary = mj_controller::database::load_materialized_session_summary(&id)?;
                    let progress = load_child_progress(&id, &live[&id])?;
                    Ok((id, summary, progress))
                })
                .collect::<Result<Vec<_>>>()
        })
        .await??;
        let mut starts = std::collections::BTreeMap::new();
        for (id, _, _) in &summaries {
            if let Some(status) = self.start_status(id.clone()).await? {
                starts.insert(id.clone(), status);
            }
        }
        load_unanswered_wake_reports(&mut summaries).await?;
        self.resolve_child_states(&mut summaries, observations)
            .await?;
        Ok((summaries, starts))
    }

    /// What the child's worker says about the processes its last turn left.
    /// A session that cannot be running has no such processes to wait for; one
    /// that may be running but has no connected worker view is unknown, which
    /// is not the same as none.
    pub(crate) async fn live_work(
        &self,
        child_session_id: &str,
    ) -> Result<mj_core::subagent::Liveness> {
        if self
            .exports
            .session_record(child_session_id)
            .is_none_or(|record| !record_may_be_running(record.state))
        {
            return Ok(mj_core::subagent::Liveness::Known(None));
        }
        let view = self
            .session_handle(child_session_id.to_owned())
            .await?
            .map(|handle| handle.view());
        Ok(liveness_of(view.as_ref()))
    }

    async fn resolve_child_states(
        &self,
        children: &mut [(
            String,
            Option<mj_core::state::MaterializedSessionSummary>,
            ChildProgress,
        )],
        observations: Vec<ChildObservation>,
    ) -> Result<()> {
        for (
            (id, _, progress),
            ChildObservation {
                record,
                parking,
                active,
                view,
            },
        ) in children.iter_mut().zip(observations)
        {
            if parking
                && record
                    .as_ref()
                    .is_some_and(|record| record.state == SessionState::Parked)
            {
                progress.state = ChildState::WaitingForWorker;
                continue;
            }
            let connected = view
                .as_ref()
                .filter(|view| view.connected)
                .and_then(|view| view.snapshot.as_ref());
            // A projection commit can precede the actor publication for that
            // completion. Resolve the turn from one view; the retry and the
            // delivery are read together from the registry.
            if let Some(snapshot) = connected {
                use crate::server::api::WaitObservation;
                let mut observation = WaitObservation {
                    subagent: true,
                    ..Default::default()
                };
                observation.observe_worker_turn(snapshot);
                let decision = observation
                    .last_turn_outcome
                    .as_ref()
                    .filter(|turn| {
                        Some(turn.completed_ordinal) == progress.last_completed_ordinal
                            && observation.execution == MaterializedExecutionState::Idle
                    })
                    .and_then(|turn| observation.turn_end(turn));
                if decision.is_none() {
                    progress.state = ChildState::WaitingForWorker;
                    continue;
                }
                if let Some(recovery) = observation.quota_recovery
                    && progress
                        .answer
                        .as_ref()
                        .is_some_and(|turn| turn.command_id == recovery.completed_command_id)
                {
                    progress.state = ChildState::Failed {
                        state: "failed",
                        reason: recovery.notice,
                        provider_failure: true,
                    };
                    continue;
                }
            }
            let state = self
                .child_state(
                    id,
                    progress,
                    record.as_ref().map(|record| record.last_profile.as_str()),
                    connected.is_some(),
                )
                .await?;
            progress.state = if active
                && connected.is_none()
                && matches!(state, ChildState::NoFailure | ChildState::Failed { .. })
            {
                ChildState::WaitingForWorker
            } else {
                state
            };
        }
        Ok(())
    }

    async fn child_state(
        &self,
        id: &str,
        progress: &ChildProgress,
        profile: Option<&str>,
        actor_connected: bool,
    ) -> Result<ChildState> {
        use mj_core::credentials::CredentialSyncCause;
        if let Some(error) = &progress.wake_error {
            let (detail, evidence) = mj_core::subagent::WakeFailure::decode(error);
            if let Some(mj_core::subagent::WakeFailure::Authentication {
                profile_id,
                staged_login,
            }) = evidence
            {
                let current = self.current_child_profile_login(&profile_id).await?;
                let verdict = self
                    .rejected_logins
                    .lock()
                    .map_err(|_| anyhow!("refused logins lock poisoned"))?
                    .wake_verdict(&staged_login, current.as_ref());
                return Ok(ChildState::from_login_verdict(profile_id, verdict, detail));
            }
            return Ok(ChildState::RetryableFailure {
                reason: detail.to_owned(),
            });
        }
        let Some(turn) = progress.report_answer.as_ref().or(progress.answer.as_ref()) else {
            return Ok(ChildState::NoFailure);
        };
        let failure_message = if progress.report_answer.is_some() {
            progress.report_failure_message.as_deref()
        } else {
            progress.failure_message.as_deref()
        };
        let Some((state, mut reason)) = mj_core::subagent::failed_turn(turn, failure_message)
        else {
            return Ok(ChildState::NoFailure);
        };
        if let (Some(profile), Some(cause)) =
            (profile, CredentialSyncCause::for_failed_turn(id, turn))
        {
            let id_for_lookup = id.to_owned();
            let command = turn.command_id.clone();
            let selection = blocking("load the failed turn's login profile", move || {
                mj_controller::database::load_session_turn_profile_model(&id_for_lookup, &command)
            })
            .await?;
            let profile = selection.map_or_else(|| profile.to_owned(), |(profile, _)| profile);
            let current = self.current_child_profile_login(&profile).await?;
            let verdict = self
                .rejected_logins
                .lock()
                .map_err(|_| anyhow!("refused logins lock poisoned"))?
                .failure_verdict(&profile, &cause, current.as_ref(), actor_connected);
            return Ok(ChildState::from_login_verdict(profile, verdict, &reason));
        }
        let diagnostic = turn.diagnostic.as_ref().filter(|diagnostic| {
            state == "failed"
                && mj_controller::profile_health::ProfileFault::of(diagnostic).is_some()
        });
        let provider_failure = if let Some(diagnostic) = diagnostic {
            if let Some(description) = self
                .describe_child_profile_failure(id, turn.command_id.clone(), diagnostic.clone())
                .await?
            {
                reason = description;
            }
            true
        } else {
            false
        };
        Ok(ChildState::Failed {
            state,
            reason,
            provider_failure,
        })
    }

    async fn current_child_profile_login(
        &self,
        profile_id: &str,
    ) -> Result<Option<mj_core::credentials::LoginFingerprint>> {
        let Ok(config) = self.profile_catalog.live() else {
            return Ok(None);
        };
        let Some(profile) = config.profiles.get(profile_id) else {
            return Ok(None);
        };
        match crate::pollers::current_profile_login(profile).await {
            Ok(current) => Ok(current),
            Err(error) => {
                tracing::warn!(%profile_id, %error, "failed to read the child's current profile login");
                Ok(None)
            }
        }
    }

    async fn describe_child_profile_failure(
        &self,
        id: &str,
        command: String,
        diagnostic: mj_core::diagnostic::TurnDiagnostic,
    ) -> Result<Option<String>> {
        let id = id.to_owned();
        let selection = blocking("load failed child turn profile", move || {
            let selection =
                mj_controller::database::load_session_turn_profile_model(&id, &command)?;
            // Older turns may predate stored selections. Prefer the
            // session's current profile, with the child model as fallback.
            let child = mj_controller::database::load_subagent(&id)?;
            let oracle = child
                .as_ref()
                .is_some_and(|child| child.kind == mj_core::subagent::SubagentKind::Oracle);
            Ok(match selection {
                Some((profile, model @ Some(_))) => Some((profile, model, oracle)),
                Some((profile, None)) => child.map(|child| (profile, child.model, oracle)),
                None => child.map(|child| (child.profile_id, child.model, oracle)),
            })
        })
        .await?;
        let Some((profile, model, oracle)) = selection else {
            return Ok(None);
        };
        let health = mj_controller::profile_health::shared()
            .lock()
            .map_err(|_| anyhow!("profile health lock poisoned"))?;
        let description = model
            .as_deref()
            .and_then(|model| health.unhealthy(&profile, model, std::time::SystemTime::now()))
            .map(|mark| format!("profile {profile} is {}", mark.describe()))
            .unwrap_or_else(|| {
                let fault = match mj_controller::profile_health::ProfileFault::of(&diagnostic) {
                    Some(mj_controller::profile_health::ProfileFault::RateLimited) => {
                        "was rate-limited"
                    }
                    Some(mj_controller::profile_health::ProfileFault::QuotaExhausted) => {
                        "ran out of quota"
                    }
                    _ => "failed at the provider",
                };
                format!("profile {profile} {fault}: {}", diagnostic.message)
            });
        let description = description.trim_end_matches('.');
        Ok(Some(if oracle {
            format!("{description}. Ask again with ask_oracle to start a fresh oracle.")
        } else {
            format!("{description}.")
        }))
    }

    /// Resolve every child that can still be reported, and the existing
    /// finished predicate, from one current observation for a parent
    /// notification. A cleanly stopped child is never news (see
    /// [`subagent_status`]), so its record is not read; a parent can hold
    /// thousands of them.
    async fn notification_children(
        &self,
        parent_id: &str,
    ) -> Result<(
        subagent_input::InputProgress,
        Vec<NotificationChildSnapshot>,
    )> {
        let inputs = self.subagent_input_progress(parent_id).await?;
        let parent = parent_id.to_owned();
        let ids = blocking("list the parent's children", move || {
            mj_controller::database::list_subagent_ids(&parent)
        })
        .await?
        .into_iter()
        .filter(|id| {
            !self
                .exports
                .session_record(id)
                .is_some_and(|record| record.is_cleanly_stopped())
        })
        .collect::<Vec<_>>();
        let parent = parent_id.to_owned();
        let relations = blocking("load the parent's reportable children", move || {
            mj_controller::database::list_subagents_among(&parent, &ids)
        })
        .await?;
        let ids = relations
            .iter()
            .map(|relation| relation.child_session_id.clone())
            .collect::<Vec<_>>();
        let (observed, starts) = self.child_snapshots(ids).await?;
        let mut observed = observed
            .into_iter()
            .map(|(id, summary, progress)| (id, (summary, progress)))
            .collect::<BTreeMap<_, _>>();
        let children = relations
            .into_iter()
            .map(|relation| {
                let id = relation.child_session_id.clone();
                let (summary, progress) = observed
                    .remove(&id)
                    .expect("child snapshots include every registered child");
                let record = self.exports.session_record(&id);
                let (state, output, finished) = inputs.status(
                    &id,
                    record.as_ref(),
                    subagent_status(
                        record.as_ref(),
                        summary.as_ref(),
                        starts.get(&id),
                        None,
                        self.exports.close_is_requested(&id),
                        &progress,
                    ),
                );
                let identity =
                    finished.then(|| finish_identity(&state, output.as_deref(), &progress));
                NotificationChildSnapshot {
                    relation,
                    progress,
                    record,
                    state,
                    output,
                    finished,
                    identity,
                }
            })
            .collect();
        Ok((inputs, children))
    }

    /// Commit each new finish and its durable mailbox event together.
    pub(crate) async fn reconcile_parent_reports(&self, parent_id: &str) -> Result<()> {
        self.reconcile_children(parent_id).await.map(|_| ())
    }

    /// Reconcile the parent's reports and say, for one child's finish at
    /// `ordinal`, whether the parent has been told. The report and the
    /// `reported_finish` marker it moves are written in one transaction, so that
    /// marker is the fact: it is read from the store after the reconcile, not
    /// from the committed snapshot, which refreshes after the write returns.
    pub(crate) async fn reconcile_child_report(
        &self,
        parent_id: &str,
        child_id: &str,
        ordinal: u64,
    ) -> Result<ChildReport> {
        let reconciled = self.reconcile_children(parent_id).await?;
        if reconciled.parent_closing {
            return Ok(ChildReport::NotNews("the parent is closing"));
        }
        match reconciled.children.get(child_id) {
            None => self.unreconciled_child_report(parent_id, child_id).await,
            Some(ChildDisposition::NotNews(why)) => Ok(ChildReport::NotNews(why)),
            Some(ChildDisposition::NotFinished) => Ok(ChildReport::NotFinished),
            Some(ChildDisposition::Reported) => {
                let child = child_id.to_owned();
                let marker = blocking("read the child's reported finish", move || {
                    mj_controller::database::load_subagent(&child)
                })
                .await?
                .and_then(|relation| relation.reported_finish);
                Ok(if marker.is_some_and(|marker| marker.covers(ordinal)) {
                    ChildReport::Told
                } else {
                    ChildReport::Awaiting
                })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveChildSummary {
    pub child_session_id: String,
    pub task_name: String,
}

pub(crate) fn orchestrator_active_children_sync(
    orchestrator_id: &str,
) -> Result<Vec<ActiveChildSummary>> {
    let children = mj_controller::database::list_subagents(orchestrator_id)?;
    let pending_finished =
        mj_controller::database::pending_subagent_finished_children(orchestrator_id)?;
    let state = mj_controller::database::load_state()?;
    let mut active = Vec::new();
    for child in children {
        if child.kind != mj_core::subagent::SubagentKind::Worker {
            continue;
        }
        let has_live_worker = state
            .sessions
            .get(&child.child_session_id)
            .is_some_and(|session| session.state.has_live_worker());
        let has_pending_finish = pending_finished.contains(&child.child_session_id);
        if has_live_worker || has_pending_finish {
            active.push(ActiveChildSummary {
                child_session_id: child.child_session_id,
                task_name: child.task_name,
            });
        }
    }
    Ok(active)
}

impl ApiBackend {
    pub(crate) async fn orchestrator_active_children(
        &self,
        orchestrator_id: &str,
    ) -> Result<Vec<ActiveChildSummary>> {
        let orchestrator_id = orchestrator_id.to_owned();
        blocking("load orchestrator active children", move || {
            orchestrator_active_children_sync(&orchestrator_id)
        })
        .await
    }

    async fn reconcile_children(&self, parent_id: &str) -> Result<Reconciled> {
        let mut reconciled = Reconciled::default();
        if self.exports.close_is_requested(parent_id) {
            reconciled.parent_closing = true;
            return Ok(reconciled);
        }
        let (inputs, children) = self.notification_children(parent_id).await?;
        // Each child has one of a few ends here: not news (and why), not seen as
        // finished, held for its facts, or its finish is reported (by an earlier
        // reconcile or by this one). Whatever its finish, every message to a
        // child that is news and failed is a notice of its own.
        //
        // A finish is decided without the parent's actor: its report waits in
        // the durable outbox, and a child's park must not wait on the parent
        // (3c8973b87). Failed deliveries are decided only from a parent queue
        // that was read: an unread queue holds failures that are not absent.
        let mut ready = Vec::new();
        let mut failures = Vec::new();
        for child in children {
            let id = child.relation.child_session_id.clone();
            let disposition = if child.relation.kind == mj_core::subagent::SubagentKind::Reviewer {
                ChildDisposition::NotNews("a reviewer is reported by its review")
            } else if self.exports.close_is_requested(&id) {
                ChildDisposition::NotNews("a close of the child was requested")
            } else if matches!(child.state.as_str(), "stopping" | "stopped") {
                ChildDisposition::NotNews("the child is stopping or stopped")
            } else if self.exports.child_review(id.clone()).await.holds() {
                // A child whose review is open is still pending: it may end
                // turns while it waits, and handback refuses until the verdict.
                ChildDisposition::NotNews("the child awaits its review's verdict")
            } else if child.relation.orchestrator
                && !self.orchestrator_active_children(&id).await?.is_empty()
            {
                ChildDisposition::NotNews("the orchestrator awaits its sub-agents")
            } else {
                if inputs.queue_known() {
                    for failed in inputs.failed_deliveries(&id) {
                        failures.push(failed_delivery_notice(&inputs, &child, &failed)?);
                    }
                }
                // A queued handback holds only the finish, and only as "not
                // finished yet": its report is still to come, so completion
                // retries the park rather than settling the child, while failed
                // messages to the child are reported as usual.
                let handback_pending = child.identity.is_some()
                    && child.relation.reported_finish.as_ref() != child.identity.as_ref()
                    && self.child_handback_pending(&id).await?;
                match &child.identity {
                    _ if handback_pending => ChildDisposition::NotFinished,
                    None => ChildDisposition::NotFinished,
                    Some(identity) if child.relation.reported_finish.as_ref() == Some(identity) => {
                        ChildDisposition::Reported
                    }
                    Some(_) => {
                        ready.push(child);
                        continue;
                    }
                }
            };
            reconciled.children.insert(id, disposition);
        }
        // A marker stored before #1459 can hold this very finish with a failed
        // delivery folded into it. Its own report row says so, whatever the
        // parent's queue shows now.
        let folded = ready
            .iter()
            .filter_map(|child| {
                let reported = child.relation.reported_finish.clone().filter(|reported| {
                    matches!(
                        reported,
                        mj_core::subagent::SubagentFinishIdentity::Terminal {
                            detail: Some(_),
                            ..
                        }
                    )
                })?;
                Some((
                    child.relation.child_session_id.clone(),
                    reported,
                    child.identity.clone()?,
                ))
            })
            .collect::<Vec<_>>();
        if !folded.is_empty() {
            let parent = parent_id.to_owned();
            let told = blocking("recognize finishes told before #1459", move || {
                mj_controller::database::finishes_told_by_folded_markers(&parent, folded)
            })
            .await?;
            ready.retain(|child| {
                let id = &child.relation.child_session_id;
                let keep = !told.contains(id);
                if !keep {
                    reconciled
                        .children
                        .insert(id.clone(), ChildDisposition::Reported);
                }
                keep
            });
        }
        // Told failures stay listed until they leave the delivery window, so
        // the store, not this snapshot, says which are news.
        if !failures.is_empty() {
            let parent = parent_id.to_owned();
            failures = blocking("find untold failed child messages", move || {
                mj_controller::database::untold_failed_deliveries(&parent, failures)
            })
            .await?;
        }
        if ready.is_empty() && failures.is_empty() {
            return Ok(reconciled);
        }
        let spans = ready
            .iter()
            .map(|child| {
                (
                    child.relation.child_session_id.clone(),
                    child.progress.report_span(),
                )
            })
            .collect::<Vec<_>>();
        let reports = if spans.is_empty() {
            BTreeMap::new()
        } else {
            blocking("load finished child reports", move || {
                mj_controller::database::load_child_answer_messages(&spans)
            })
            .await?
        };
        let mut notifications = Vec::with_capacity(ready.len());
        let mut enqueued = Vec::with_capacity(ready.len());
        for child in ready {
            let id = &child.relation.child_session_id;
            enqueued.push(id.clone());
            let output = if child.state == "completed" {
                match &child.progress.report {
                    ReportState::Delivered(message) => Some(message.clone()),
                    _ => reports.get(id).cloned().flatten().or(child.output),
                }
            } else {
                child.output
            };
            let oracle = child.relation.kind == mj_core::subagent::SubagentKind::Oracle;
            let mut entry = wait_agent_entry(
                id,
                &child.state,
                output,
                child.finished,
                &child.progress,
                oracle,
            );
            entry["child_name"] =
                live_child_name(child.record.as_ref(), &child.relation.task_name).into();
            entry["task_name"] = child.relation.task_name.into();
            inputs.annotate(id, &mut entry);
            mark_parked(&mut entry, child.record.as_ref());
            mark_promptable(
                &mut entry,
                &child.state,
                &child.progress,
                child.record.as_ref(),
            );
            let report_has_facts = child.finished && !oracle;
            let mut report =
                serde_json::from_value::<mj_core::mailbox::SubagentFinishedReport>(entry)?;
            // What the child's worker observed when the turn ended is part of the
            // outcome that ended it, so the report reads it from the same snapshot
            // as the finish, and nothing waits on it.
            if report_has_facts {
                report.facts = Some(finish_facts(&child.progress));
            }
            notifications.push((
                child.identity.expect("ready report has a finish identity"),
                report,
            ));
        }
        let parent = parent_id.to_owned();
        blocking("enqueue finished child reports", move || {
            mj_controller::database::enqueue_subagent_reports(parent, notifications, failures)
        })
        .await?;
        for id in enqueued {
            reconciled.children.insert(id, ChildDisposition::Reported);
        }
        Ok(reconciled)
    }

    /// What `reconcile_children` would have said about a child it skipped:
    /// one that is cleanly stopped, or that is not this parent's child at all.
    /// The checks keep `reconcile_children`'s order.
    async fn unreconciled_child_report(
        &self,
        parent_id: &str,
        child_id: &str,
    ) -> Result<ChildReport> {
        let relation = self.owned_relation(parent_id, child_id).await?;
        Ok(ChildReport::NotNews(match relation {
            None => "the child is not a worker of this parent",
            Some(relation) if relation.kind == mj_core::subagent::SubagentKind::Reviewer => {
                "a reviewer is reported by its review"
            }
            Some(_) if self.exports.close_is_requested(child_id) => {
                "a close of the child was requested"
            }
            Some(_) => "the child is stopping or stopped",
        }))
    }

    /// The child's relation when it belongs to this parent.
    async fn owned_relation(
        &self,
        parent_id: &str,
        child_id: &str,
    ) -> Result<Option<mj_core::subagent::SubagentRecord>> {
        let child = child_id.to_owned();
        Ok(blocking("read the child's relation", move || {
            mj_controller::database::load_subagent(&child)
        })
        .await?
        .filter(|relation| relation.parent_session_id == parent_id))
    }

    async fn require_owned_child(&self, parent_id: &str, child_id: &str) -> Result<()> {
        anyhow::ensure!(
            self.owned_relation(parent_id, child_id)
                .await?
                .is_some_and(|child| child.kind == mj_core::subagent::SubagentKind::Worker),
            "session {child_id} does not belong to parent {parent_id}"
        );
        Ok(())
    }

    async fn resolve_owned_child(&self, parent_id: &str, requested_id: &str) -> Result<String> {
        let parent_id = parent_id.to_owned();
        let requested_id = requested_id.to_owned();
        blocking("resolve sub-agent close target", move || {
            let state = Controller::load()?.state;
            let child_id =
                subagent_input::resolve_session_target(&state, Some(&parent_id), &requested_id)?;
            anyhow::ensure!(
                state.subagents.get(&child_id).is_some_and(|relation| {
                    relation.parent_session_id == parent_id
                        && relation.kind == mj_core::subagent::SubagentKind::Worker
                }),
                "session {requested_id} does not belong to parent {parent_id}"
            );
            Ok(child_id)
        })
        .await
    }

    /// Decide what a child that ended its turn owes before its report can go
    /// to the parent, and send the one prompt that decision calls for: a
    /// handback reminder, or, for a child held while processes it started
    /// run, the follow-up that is due. The parent's completion report waits
    /// for that prompt's turn, including after daemon recovery.
    ///
    /// The caller passes the turn, the worker's liveness and what is queued
    /// or running from the live snapshot it just saw: the store's copy can
    /// lag it, and deciding from an older turn could skip a prompt the parent
    /// needs.
    pub async fn remind_subagent_to_hand_back(
        &self,
        child_session_id: &str,
        handback_tool: bool,
        last_turn: &mj_core::state::MaterializedTurnOutcome,
        liveness: &mj_core::subagent::Liveness,
        in_flight: &[String],
    ) -> Result<ReportNudge> {
        // A prompt already queued or running is the child's next task; its
        // report supersedes this turn's. A queued hold follow-up stays judged:
        // if the queue drops it, the next observation sends it again.
        if handback_tool
            && in_flight
                .iter()
                .any(|id| mj_core::subagent::hold_follow_up_of(id).is_some())
        {
            return Ok(ReportNudge::FollowUpSent);
        }
        if !handback_tool || !in_flight.is_empty() {
            return Ok(ReportNudge::NotOwed);
        }
        let report = blocking("load sub-agent report", {
            let child_id = child_session_id.to_owned();
            move || mj_controller::database::load_subagent_report(&child_id)
        })
        .await?;
        let state = mj_core::subagent::report_state(
            handback_tool,
            &report,
            Some(last_turn),
            liveness,
            &[],
            mj_core::clock::epoch_millis(),
        );
        let (command_id, command) = match state {
            ReportState::Pending { remind: true } => (
                mj_core::subagent::handback_reminder_command_id(last_turn.completed_ordinal),
                RelayCommand::HandbackReminder {
                    completed_command_id: last_turn.command_id.clone(),
                    completed_ordinal: last_turn.completed_ordinal,
                    // Only a worker that knows the field may be sent it.
                    running: self
                        .reminder_running_lines(child_session_id, last_turn)
                        .await,
                },
            ),
            ReportState::Pending { remind: false } => return Ok(ReportNudge::Pending),
            ReportState::Held {
                give_up_at_ms,
                follow_up: None,
                ..
            } => return Ok(ReportNudge::Holding { give_up_at_ms }),
            ReportState::Held {
                waiting_on,
                follow_up: Some(kind),
                ..
            } => (
                mj_core::subagent::hold_follow_up_command_id(kind, last_turn.completed_ordinal),
                RelayCommand::HoldFollowUp {
                    completed_command_id: last_turn.command_id.clone(),
                    completed_ordinal: last_turn.completed_ordinal,
                    kind,
                    running: match kind {
                        mj_core::subagent::HoldFollowUpKind::ProcessesExited => Vec::new(),
                        mj_core::subagent::HoldFollowUpKind::GiveUp => waiting_on,
                    },
                },
            ),
            ReportState::Delivered(_) | ReportState::Fallback => return Ok(ReportNudge::NotOwed),
        };
        let is_follow_up = matches!(command, RelayCommand::HoldFollowUp { .. });
        let submitted = async {
            let handle = self
                .sessions
                .session(child_session_id.to_owned())
                .await
                .with_context(|| format!("session {child_session_id} is not running"))?;
            handle.submit(command_id.clone(), command).await
        }
        .await;
        let child_id = child_session_id.to_owned();
        let for_command_id = last_turn.command_id.clone();
        match submitted {
            Ok(_) if is_follow_up => Ok(ReportNudge::FollowUpSent),
            Ok(_) => {
                let reminder = mj_core::subagent::HandbackReminder {
                    command_id: command_id.clone(),
                    for_command_id,
                    sent_at_ms: mj_core::clock::epoch_millis(),
                };
                blocking("record handback reminder", move || {
                    mj_controller::database::record_handback_reminder(&child_id, &reminder)
                })
                .await?;
                Ok(ReportNudge::Pending)
            }
            Err(error) => {
                if error
                    .downcast_ref::<mj_client::session::DeliveryUnconfirmed>()
                    .is_some()
                {
                    // Retry the same guarded command and retained receipt. A lost
                    // acknowledgement does not prove the prompt was refused.
                    return Err(error);
                }
                if is_follow_up {
                    // Only the worker's own refusal says the completion is no
                    // longer current. Anything else (a worker that cannot take
                    // a prompt right now, a session that is not running, a
                    // transport error) did not decide anything: the child stays
                    // held and the loop retries with backoff.
                    if error
                        .downcast_ref::<mj_client::session::Refused>()
                        .is_none()
                    {
                        return Err(error.context("could not prompt a held sub-agent"));
                    }
                    tracing::info!(
                        child_session_id,
                        error = format!("{error:#}"),
                        "a held sub-agent moved on before its follow-up; its last turn is reported"
                    );
                    blocking("record refused hold follow-up", move || {
                        mj_controller::database::record_handback_reminder_failed(
                            &child_id,
                            &for_command_id,
                        )
                    })
                    .await?;
                    return Ok(ReportNudge::NotOwed);
                }
                tracing::warn!(
                    child_session_id,
                    error = format!("{error:#}"),
                    "could not prompt a sub-agent to hand back its report"
                );
                // Its last message stands as its report when the reminder fails.
                blocking("record failed handback reminder", move || {
                    mj_controller::database::record_handback_reminder_failed(
                        &child_id,
                        &for_command_id,
                    )
                })
                .await?;
                Ok(ReportNudge::NotOwed)
            }
        }
    }

    /// The processes a handback reminder names, for a child whose worker can
    /// carry them (relay protocol 67 or later).
    async fn reminder_running_lines(
        &self,
        child_session_id: &str,
        last_turn: &mj_core::state::MaterializedTurnOutcome,
    ) -> Vec<String> {
        let Some(facts) = last_turn.work_facts.as_ref() else {
            return Vec::new();
        };
        let supported = match self.session_handle(child_session_id.to_owned()).await {
            Ok(Some(handle)) => handle
                .view()
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.operational.relay_protocol_version)
                .is_some_and(|version| version >= mj_core::relay::RELAY_WORK_LIVENESS_PROTOCOL),
            _ => false,
        };
        if supported {
            mj_core::subagent::running_process_lines(facts)
        } else {
            Vec::new()
        }
    }

    /// Whether the park of a child that was just reported waits for its
    /// parent's turn. It does while the processes the child's last turn left
    /// are alive and the parent is still running the turn that received (or
    /// will receive) the child's report: the parent may tell the child to
    /// continue, and a park stops those processes.
    ///
    /// The parent's worker owns the delivery facts, read from its live
    /// snapshot each time; the outbox row says whether it accepted the report.
    /// A parent that is not running, or a child whose processes are gone,
    /// opens the gate.
    pub async fn park_waits_for_parent_turn(
        &self,
        parent_session_id: &str,
        child_session_id: &str,
        completed_ordinal: u64,
        liveness: Option<&mj_core::work_facts::WorkLiveness>,
    ) -> Result<bool> {
        if !liveness.is_some_and(|liveness| {
            liveness.completed_ordinal == completed_ordinal && liveness.alive > 0
        }) {
            return Ok(false);
        }
        if self
            .exports
            .session_record(parent_session_id)
            .is_none_or(|record| record.state != SessionState::Running)
        {
            return Ok(false);
        }
        let (parent, child) = (parent_session_id.to_owned(), child_session_id.to_owned());
        let Some(mail) = blocking("load child report mail", move || {
            mj_controller::database::load_report_mail(&parent, &child)
        })
        .await?
        else {
            return Ok(false);
        };
        let view = self
            .session_handle(parent_session_id.to_owned())
            .await?
            .map(|handle| handle.view());
        Ok(!report_turn_ended(
            &mail.key,
            mail.accepted_ordinal,
            view.as_ref().and_then(|view| view.snapshot.as_ref()),
        ))
    }

    /// Return the worker's atomic idle reservation outcome. The coordinator
    /// retains a busy park separately from the already delivered notice.
    pub async fn park_subagent(
        &self,
        child_session_id: &str,
    ) -> Result<mj_controller::controller::ParkOutcome> {
        Arc::clone(&self.exports)
            .park_subagent(child_session_id.to_owned())
            .await
    }

    /// Refuse an export that needs the target when the session no longer has
    /// one. The record drops its locator on stop, so this is the honest answer
    /// rather than a command that cannot be addressed anywhere.
    fn require_live_target(&self, session_id: &str) -> Result<(), ExportError> {
        let record = self
            .exports
            .session_record(session_id)
            .ok_or_else(|| ExportError::Refused(format!("unknown session {session_id}")))?;
        match record.target {
            Some(_) => Ok(()),
            None => Err(ExportError::Refused(format!(
                "session {session_id} has no live target; resume it first"
            ))),
        }
    }

    /// Refuse a push while the agent is still working. A push mid-turn would
    /// publish a tree the agent is in the middle of changing.
    async fn require_idle_turn(&self, session_id: &str) -> Result<(), ExportError> {
        let Ok(handle) = self.sessions.session(session_id.to_owned()).await else {
            return Ok(());
        };
        let Some(snapshot) = handle.view().snapshot else {
            return Ok(());
        };
        let running = matches!(
            snapshot.materialized.execution,
            MaterializedExecutionState::Running { .. }
        ) || snapshot.materialized.active_turn.is_some();
        if running {
            return Err(ExportError::Refused(
                "this session is running a turn; cancel or wait for it before pushing".into(),
            ));
        }
        Ok(())
    }
}

/// Whether a session in this state may have a worker running its turn.
pub(crate) fn record_may_be_running(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Provisioning
            | SessionState::Running
            | SessionState::Disconnected
            | SessionState::Checkpointing
    )
}

async fn pending_spawn_entries(
    parent: &str,
    snapshot: Option<&mj_core::state::ManagedSessionSnapshot>,
) -> Result<BTreeMap<String, serde_json::Value>> {
    use mj_core::subagent::SubagentToolAction;
    let mut requests = Vec::new();
    if let Some(snapshot) = snapshot {
        for request in snapshot.subagent_requests.iter() {
            if let SubagentToolAction::Spawn { task_name, .. }
            | SubagentToolAction::SpawnOrchestrator { task_name, .. } = &request.action
            {
                requests.push((request.request_id.clone(), task_name.clone()));
            }
        }
    }
    if requests.is_empty() {
        return Ok(BTreeMap::new());
    }
    let parent = parent.to_owned();
    blocking("read pending spawns", move || {
        let mut pending = BTreeMap::new();
        for (id, task_name) in requests {
            let mut entry = serde_json::json!({
                "request_id": id,
                "task_name": task_name,
                "status": "pending_registration",
            });
            if let Some((_, Some(stored))) = mj_controller::database::load_delegation(&parent, &id)?
            {
                if !stored.result.is_error {
                    continue;
                }
                entry["status"] = "failed".into();
                entry["error"] = stored.result.message.into();
            }
            pending.insert(id, entry);
        }
        Ok(pending)
    })
    .await
}

/// The liveness a connected worker view publishes; without one it is unknown.
pub(crate) fn liveness_of(
    view: Option<&mj_client::session::ManagedSessionView>,
) -> mj_core::subagent::Liveness {
    match view
        .filter(|view| view.connected)
        .and_then(|view| view.snapshot.as_ref())
    {
        Some(snapshot) => {
            mj_core::subagent::Liveness::Known(snapshot.operational.work_liveness.clone())
        }
        None => mj_core::subagent::Liveness::Unknown,
    }
}

/// Lists profiles that could not be checked, each with its reason.
fn format_unavailable_reasons(unavailable: &[(String, String)]) -> String {
    unavailable
        .iter()
        .map(|(id, reason)| format!("{id} ({reason})"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// Whether the parent turn that received a child's report has ended: the
/// parent's worker accepted the event, the snapshot shows that acceptance, the
/// event is no longer waiting for the agent, and the parent is idle. A parent
/// that is idle with the event delivered ended the turn it was delivered in;
/// one with the event still waiting is about to run it.
fn report_turn_ended(
    key: &str,
    accepted_ordinal: Option<u64>,
    parent: Option<&mj_core::state::ManagedSessionSnapshot>,
) -> bool {
    let (Some(accepted), Some(snapshot)) = (accepted_ordinal, parent) else {
        return false;
    };
    snapshot.operational.latest_ordinal >= accepted
        && !snapshot
            .operational
            .undelivered_mailbox_event_keys
            .iter()
            .any(|undelivered| undelivered == key)
        && matches!(
            snapshot.materialized.execution,
            MaterializedExecutionState::Idle
        )
        && snapshot.materialized.active_turn.is_none()
        && snapshot.materialized.queued_prompts.is_empty()
}

/// What a child owes before its report can go to the parent, after the prompt
/// that was due has been sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportNudge {
    /// Nothing is owed: report the turn as it stands.
    NotOwed,
    /// A reminder is on its way or already ran; the child's next turn is what
    /// will be judged.
    Pending,
    /// A hold follow-up is queued or was just sent. Unlike a reminder it is
    /// not marked done: a queue that drops it is seen on the next observation
    /// and it is sent again under the same id.
    FollowUpSent,
    /// The child waits on processes it started. Nothing is reported or
    /// stopped before this time, unless the worker's liveness changes first.
    Holding { give_up_at_ms: i64 },
}

/// The notice that tells a parent one message to its child was not delivered:
/// the child's own state, whether it can take the message again, what is still
/// queued for it, and that one failed delivery. What the child's finish says
/// (its output, facts and failure) is its finish report's to tell.
///
/// A question to the oracle takes the oracle's report path, which names no
/// session and points to `ask_oracle`; `send_message` refuses the oracle.
/// Every worker's oracle path heads a report "answered" only for the state
/// `completed`, so the notice carries the question's outcome as its state.
fn failed_delivery_notice(
    inputs: &subagent_input::InputProgress,
    child: &NotificationChildSnapshot,
    failed: &subagent_input::FailedDelivery<'_>,
) -> Result<mj_controller::database::FailedDeliveryNotice> {
    let id = &child.relation.child_session_id;
    if child.relation.kind == mj_core::subagent::SubagentKind::Oracle {
        let entry = serde_json::json!({
            "oracle": true,
            "child_session_id": id,
            "task_name": child.relation.task_name,
            "state": "not-delivered",
            "finished": false,
            "failure": {
                "kind": "question_not_delivered",
                "summary": format!(
                    "Your question was not delivered: {}. Ask again with ask_oracle.",
                    failed.error.trim_end_matches('.')
                ),
            },
            "message_deliveries": [failed.delivery],
            "output": null,
            "report_dir": null,
        });
        return Ok(mj_controller::database::FailedDeliveryNotice {
            request_id: failed.request_id.to_owned(),
            report: serde_json::from_value(entry)?,
        });
    }
    let mut entry = serde_json::json!({
        "child_session_id": id,
        "child_name": live_child_name(child.record.as_ref(), &child.relation.task_name),
        "task_name": child.relation.task_name,
        "state": child.state,
        "finished": false,
        "message_deliveries": [failed.delivery],
        "output": null,
        "report_dir": null,
    });
    inputs.annotate_pending(id, &mut entry);
    mark_parked(&mut entry, child.record.as_ref());
    mark_promptable(
        &mut entry,
        &child.state,
        &child.progress,
        child.record.as_ref(),
    );
    Ok(mj_controller::database::FailedDeliveryNotice {
        request_id: failed.request_id.to_owned(),
        report: serde_json::from_value(entry)?,
    })
}

/// What makes a child's finish new for its parent. A failed message delivery
/// is not part of it: it keeps the child's own state (#1381) and is told by a
/// notice of its own (#1459), so a failure never hides, moves or repeats a
/// finish.
fn finish_identity(
    state: &str,
    output: Option<&str>,
    progress: &ChildProgress,
) -> mj_core::subagent::SubagentFinishIdentity {
    if progress.wake_error.is_none()
        && let Some((start_position, completed_ordinal)) = progress.finished_span
    {
        return mj_core::subagent::SubagentFinishIdentity::Turn {
            start_position,
            completed_ordinal,
            state: state.to_owned(),
        };
    }
    mj_core::subagent::SubagentFinishIdentity::Terminal {
        state: state.to_owned(),
        detail: output.map(str::to_owned),
        last_completed_ordinal: progress.last_completed_ordinal,
    }
}

/// Classify a child session and choose the output its parent reads.
///
/// `finished_turn_message` is what the child's last finished turn answered.
/// A finished child reports that, not the newest agent message anywhere in its
/// session: a harness records messages outside any turn — a resume notice, for
/// one — and those would otherwise stand in for the child's report. It falls
/// back to the session-wide message only for a child with no recorded turn
/// span, which has no report of its own to lose. A running child keeps showing
/// its newest message, which is the point of looking at a running child.
///
/// `closing` says a close the daemon admitted is still running for this child.
/// A close is not instant: it cancels whatever owned the session, persists its
/// final conversation, seals the relay and tears the child's process tree down.
/// The record only says `Closing` once that is under way. Without this, a child whose turn had
/// finished reported `completed` the moment its close was admitted, so a parent
/// that closed a child and spawned its replacement stacked the two process
/// trees inside one container (#1087). This is the projection the daemon's own
/// viewer applies, down to leaving a record that already says `Stopped` alone.
fn subagent_status(
    record: Option<&mj_core::state::SessionRecord>,
    summary: Option<&mj_core::state::MaterializedSessionSummary>,
    start: Option<&StartStatus>,
    finished_turn_message: Option<&str>,
    closing: bool,
    progress: &ChildProgress,
) -> (String, Option<String>, bool) {
    // A close the daemon admitted owns this child until it finishes, the same
    // rule `resolve_wait` applies to a session-level wait: a close ends
    // nothing. It is read before anything else for the same reason it is there,
    // so a child that had finished — or had failed to start — is not reported
    // as something the parent is done with while its teardown is still running.
    // A record that already settled to `Stopped` is left alone, exactly as the
    // daemon's viewer projection leaves it.
    if record.is_some_and(|record| record.state == SessionState::StartupCleanup) {
        return (
            "stopping".into(),
            record.and_then(|record| record.last_error.clone()),
            false,
        );
    }
    if closing && record.is_some_and(|record| record.state != SessionState::Stopped) {
        return ("stopping".into(), None, false);
    }
    // A closed child is stopped, whatever its first prompt did. The close
    // cleared the launch failure from the record, so a failed first prompt's
    // message would otherwise replace it, keep the child `error`, and reach
    // the parent as a second failure (#1321). A stopped record that holds an
    // error is a failed resume, which still reports below.
    if record.is_some_and(mj_core::state::SessionRecord::is_cleanly_stopped) {
        return ("stopped".into(), None, true);
    }
    // A record that failed to start holds the cause; the follow-up's own
    // message only says that the session would not take a prompt, which is
    // the symptom. Prefer the cause when there is one, and keep the follow-up
    // message for a session whose record is fine and whose first prompt or
    // selector was the thing that failed.
    let recorded_cause = record
        .filter(|record| record.state == SessionState::Error)
        .and_then(|record| record.last_error.clone());
    if let Some(StartStatus::Failed { message }) = start {
        return (
            "error".into(),
            Some(recorded_cause.unwrap_or_else(|| message.clone())),
            true,
        );
    }
    // A failed wake owns the output even when an earlier turn delivered a report.
    if record.is_some_and(|record| record.state == SessionState::Parked)
        && progress.wake_error.is_some()
        && let Some((state, reason)) = progress.state.failure_output()
    {
        return (state.into(), Some(reason), true);
    }
    let start_pending = matches!(start, Some(StartStatus::Pending));
    let lifecycle = record.map(|record| record.state);
    // A parked child's worker was stopped only once it was idle, and nothing
    // can move its stored projection until it is started again, so it is
    // idle whatever that projection last said.
    let idle = |summary: &mj_core::state::MaterializedSessionSummary| {
        lifecycle == Some(SessionState::Parked)
            || matches!(summary.execution, MaterializedExecutionState::Idle)
    };
    match lifecycle {
        Some(SessionState::Error) => ("error".into(), recorded_cause, true),
        Some(SessionState::Lost) => ("lost".into(), None, true),
        Some(SessionState::Stopped | SessionState::DestroyedWithDataLoss) | None => {
            ("stopped".into(), None, true)
        }
        Some(SessionState::Closing | SessionState::Destroying) => ("stopping".into(), None, false),
        Some(SessionState::Provisioning) if summary.is_none() || start_pending => {
            ("preparing".into(), None, false)
        }
        _ if start_pending => ("running".into(), None, false),
        _ => match summary {
            // An idle child is not done while the parent's newest prompt is
            // unanswered, which is how it looks until the store catches up
            // with a prompt just given, or while it still owes its report and
            // Mjolnir is reminding it.
            Some(summary)
                if idle(summary)
                    && (matches!(
                        progress.state,
                        ChildState::WaitingForWorker | ChildState::Deciding | ChildState::RetryOwed
                    ) || progress.awaiting_prompt(match start {
                        Some(StartStatus::Submitted { turn_id }) => Some(*turn_id),
                        _ => None,
                    }) || matches!(
                        progress.report,
                        ReportState::Pending { .. } | ReportState::Held { .. }
                    )) =>
            {
                ("running".into(), summary.last_agent_message.clone(), false)
            }
            Some(summary) if idle(summary) => {
                if let ReportState::Delivered(message) = &progress.report {
                    ("completed".into(), Some(message.clone()), true)
                } else if let Some((state, reason)) = progress.state.failure_output() {
                    (state.into(), Some(reason), true)
                } else {
                    (
                        "completed".into(),
                        finished_turn_message.map(str::to_owned).or_else(|| {
                            progress
                                .report_answer
                                .is_none()
                                .then(|| summary.last_agent_message.clone())
                                .flatten()
                        }),
                        true,
                    )
                }
            }
            Some(summary) => ("running".into(), summary.last_agent_message.clone(), false),
            None => ("preparing".into(), None, false),
        },
    }
}

/// A child's turns as the readers see them (the committed snapshot, or the
/// store in a process without the writer): the completion ordinal of its last
/// finished turn, and whether a turn is running now.
pub(crate) fn load_child_turns(child_id: &str) -> Result<(Option<u64>, bool)> {
    let state = match mj_controller::database::committed_state()? {
        Some(committed) => committed.turns.get(child_id).map(|turn| turn.state.clone()),
        None => mj_controller::database::load_materialized_turn_outcome(child_id)?,
    };
    Ok(state.map_or((None, false), |(_, active, last)| {
        (last.map(|turn| turn.completed_ordinal), active.is_some())
    }))
}

/// What a reconcile found for each of a parent's children.
#[derive(Default)]
struct Reconciled {
    parent_closing: bool,
    children: BTreeMap<String, ChildDisposition>,
}

enum ChildDisposition {
    /// The child's finish is reported, now or earlier.
    Reported,
    /// There is no report to make, and why.
    NotNews(&'static str),
    /// The reconcile's view of the child is that it has not finished. That is not
    /// "not news": it can be a child that did finish seen through a view that is
    /// behind, or one that went on to a newer turn; the caller, which holds the
    /// ending it asks about, tells which.
    NotFinished,
}

/// Whether the parent has been told of one child's finish.
pub(crate) enum ChildReport {
    /// The report is enqueued and the child's marker covers the finish.
    Told,
    /// There is nothing to tell, for the reason given: the finish is not news.
    NotNews(&'static str),
    /// The reconcile does not see the child as finished.
    NotFinished,
    /// News, but not reported yet: the readers' snapshot has not caught up with
    /// the finish, or the enqueue skipped it. Ask again.
    Awaiting,
}

/// What the store says about a child's answer to its parent's newest prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChildProgress {
    /// The completed turn observed here owns only this transcript span.
    pub finished_span: Option<(u64, u64)>,
    /// The earlier substantive answer, when the last wake had no response.
    report_answer: Option<mj_core::state::MaterializedTurnOutcome>,
    unanswered_messages: Option<Vec<mj_controller::database::UnansweredMailboxMessage>>,
    report_failure_message: Option<String>,
    /// Handback observed with the current turn, before historical reads.
    observed_handback: Option<mj_core::subagent::SubagentHandback>,
    /// Stable terminal-failure generation even when the turn has no span.
    pub last_completed_ordinal: Option<u64>,
    /// Where the child's report stands after its last finished turn.
    pub report: ReportState,
    /// The newest prompt the parent gave the child, by acceptance ordinal.
    pub awaited_ordinal: Option<u64>,
    /// The outcome owning the child's answer.
    pub answer: Option<mj_core::state::MaterializedTurnOutcome>,
    state: ChildState,
    failure_message: Option<String>,
    wake_error: Option<String>,
    /// The directory on the parent's target where the child writes the
    /// details its report points to.
    pub report_dir: Option<String>,
}

/// The one verdict shared by reports, their metadata, and park admission.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChildState {
    NoFailure,
    /// The matching worker publication is still pending or resolves this turn.
    WaitingForWorker,
    Deciding,
    Refused {
        profile_id: String,
    },
    RetryOwed,
    RetryableFailure {
        reason: String,
    },
    Failed {
        state: &'static str,
        reason: String,
        provider_failure: bool,
    },
}

impl ChildState {
    fn from_login_verdict(
        profile_id: String,
        verdict: mj_core::credentials::LoginFailureVerdict,
        detail: &str,
    ) -> Self {
        use mj_core::credentials::LoginFailureVerdict::*;
        let reason = match verdict {
            Deciding => return Self::Deciding,
            Refused => return Self::Refused { profile_id },
            RetryOwed => return Self::RetryOwed,
            Renewed => "the login was renewed after this authentication failure".to_owned(),
            Unrefused { reason } => {
                format!("a refusal of the current login was not established ({reason})")
            }
        };
        Self::RetryableFailure {
            reason: format!(
                "For profile {profile_id}, {reason}. Use `send_message` to continue the child with its current credentials. The original error was {detail}"
            ),
        }
    }

    fn failure_output(&self) -> Option<(&'static str, String)> {
        match self {
            Self::Refused { profile_id } => Some((
                "failed",
                format!(
                    "profile {profile_id}: {}",
                    mj_core::subagent::login_invalid_reason(profile_id)
                ),
            )),
            Self::RetryableFailure { reason } => Some(("failed", reason.clone())),
            Self::Failed { state, reason, .. } => Some((state, reason.clone())),
            Self::NoFailure | Self::WaitingForWorker | Self::Deciding | Self::RetryOwed => None,
        }
    }
}

impl ChildProgress {
    /// A child whose last turn answered everything asked and ended normally,
    /// with this report.
    #[cfg(test)]
    pub(crate) fn settled(report: ReportState) -> Self {
        Self {
            finished_span: None,
            report_answer: None,
            unanswered_messages: None,
            observed_handback: None,
            report_failure_message: None,
            last_completed_ordinal: Some(1),
            report,
            awaited_ordinal: None,
            answer: None,
            state: ChildState::NoFailure,
            wake_error: None,
            failure_message: None,
            report_dir: None,
        }
    }

    /// Whether the child still owes its initial answer or the parent's newest
    /// prompt. Registration publishes an idle child before startup or prompt
    /// acceptance exists; only a recorded finished turn can end that wait.
    /// `submitted` is the first prompt's ordinal while startup remembers it.
    fn awaiting_prompt(&self, submitted: Option<u64>) -> bool {
        self.last_completed_ordinal.is_none()
            || self
                .awaited_ordinal
                .into_iter()
                .chain(submitted)
                .any(|ordinal| {
                    self.answer
                        .as_ref()
                        .is_none_or(|turn| !turn.answers(ordinal))
                })
    }

    fn report_span(&self) -> Option<(u64, u64)> {
        self.report_answer
            .as_ref()
            .and_then(|turn| {
                turn.turn_start_position
                    .map(|start| (start, turn.completed_ordinal))
            })
            .or(self.finished_span)
    }
}

/// Read historical report spans off the event loop, after observing the latest
/// turn. The worker's current turn still owns readiness and finish identity.
async fn load_unanswered_wake_reports(
    children: &mut [(
        String,
        Option<mj_core::state::MaterializedSessionSummary>,
        ChildProgress,
    )],
) -> Result<()> {
    let candidates: Vec<_> = children
        .iter()
        .filter_map(|(id, _, progress)| {
            let turn = progress.answer.as_ref()?;
            (mj_core::continuation::carries_mailbox_wake(&turn.command_id)
            && matches!(&turn.outcome, mj_core::state::TurnOutcomeKind::Completed { stop_reason }
                if stop_reason == mj_core::acp::PROMPT_UNANSWERED_STOP_REASON)
            && !progress.observed_handback.as_ref().is_some_and(|handback| handback.answers(&turn.command_id)))
            .then(|| (id.clone(), turn.clone()))
        })
        .collect();
    if candidates.is_empty() {
        return Ok(());
    }
    let reports = blocking("read unanswered mailbox wakes", move || {
        candidates
            .into_iter()
            .map(|(id, wake)| {
                let report = mj_controller::database::load_unanswered_wake_report(&id, &wake)?;
                Ok((id, report))
            })
            .collect::<Result<BTreeMap<_, _>>>()
    })
    .await?;
    for (id, _, progress) in children {
        if let Some(Some(report)) = reports.get(id) {
            if let Some(turn) = &report.turn {
                progress.report = progress
                    .observed_handback
                    .as_ref()
                    .filter(|h| h.answers(&turn.command_id))
                    .map_or(ReportState::Fallback, |h| {
                        ReportState::Delivered(h.message.clone())
                    });
                progress.report_answer = Some(turn.clone());
                progress.report_failure_message = report.failure_message.clone();
            }
            progress.unanswered_messages = Some(report.messages.clone());
        }
    }
    Ok(())
}

/// Derive one child's progress from the writer's coherent committed records.
fn child_progress(
    committed: &mj_controller::database::CommittedState,
    child_id: &str,
    liveness: &mj_core::subagent::Liveness,
) -> ChildProgress {
    let subagent = committed.state.subagents.get(child_id);
    let empty = mj_core::subagent::SubagentReport::default();
    let recorded = committed.subagent_reports.get(child_id).unwrap_or(&empty);
    let turn = committed.turns.get(child_id);
    let active = turn.and_then(|turn| turn.state.1.as_ref());
    let last = turn.and_then(|turn| turn.state.2.as_ref());
    let in_flight = active
        .into_iter()
        .map(|turn| turn.command_id.as_str())
        .collect::<Vec<_>>();
    let handback_tool = subagent.is_some_and(|record| record.handback_tool);
    ChildProgress {
        report_answer: None,
        unanswered_messages: None,
        observed_handback: recorded.handback.clone(),
        report_failure_message: None,
        finished_span: last.and_then(|turn| {
            turn.turn_start_position
                .map(|start| (start, turn.completed_ordinal))
        }),
        last_completed_ordinal: last.map(|turn| turn.completed_ordinal),
        report: mj_core::subagent::report_state(
            handback_tool,
            recorded,
            last,
            liveness,
            &in_flight,
            mj_core::clock::epoch_millis(),
        ),
        awaited_ordinal: recorded.awaited_ordinal,
        answer: last.cloned(),
        state: ChildState::NoFailure,
        wake_error: child_wake_error(committed.state.sessions.get(child_id)),
        failure_message: turn.and_then(|turn| turn.failed_message.clone()),
        report_dir: recorded.report_dir.clone(),
    }
}

/// A failed wake owns the failure even if an earlier turn delivered a report.
fn child_wake_error(record: Option<&mj_core::state::SessionRecord>) -> Option<String> {
    record
        .filter(|record| record.state == SessionState::Parked)
        .and_then(|record| record.last_error.clone())
}

/// Read a child's progress from the store. Report notifications and
/// `list_agents` apply this one rule; the session wait and the reminder apply
/// the same report rule to the turn they saw.
pub(crate) fn load_child_progress(
    child_id: &str,
    liveness: &mj_core::subagent::Liveness,
) -> Result<ChildProgress> {
    if let Some(committed) = mj_controller::database::committed_state()? {
        return Ok(child_progress(&committed, child_id, liveness));
    }
    let subagent = mj_controller::database::load_subagent(child_id)?;
    let handback_tool = subagent.as_ref().is_some_and(|record| record.handback_tool);
    let recorded = mj_controller::database::load_subagent_report(child_id)?;
    let (active, last) = mj_controller::database::load_materialized_turn_outcome(child_id)?
        .map(|(_, active, last)| (active, last))
        .unwrap_or_default();
    let in_flight = active
        .iter()
        .map(|turn| turn.command_id.as_str())
        .collect::<Vec<_>>();
    let report = mj_core::subagent::report_state(
        handback_tool,
        &recorded,
        last.as_ref(),
        liveness,
        &in_flight,
        mj_core::clock::epoch_millis(),
    );
    let record = mj_controller::database::load_session_record(child_id)?;
    let wake_error = child_wake_error(record.as_ref());
    let failure_message = if wake_error.is_none()
        && last
            .as_ref()
            .is_some_and(|turn| mj_core::subagent::failed_turn(turn, None).is_some())
    {
        mj_controller::database::load_materialized_finished_turn_message(child_id)?
    } else {
        None
    };
    Ok(ChildProgress {
        report_answer: None,
        unanswered_messages: None,
        observed_handback: recorded.handback.clone(),
        report_failure_message: None,
        finished_span: last.as_ref().and_then(|turn| {
            turn.turn_start_position
                .map(|start| (start, turn.completed_ordinal))
        }),
        last_completed_ordinal: last.as_ref().map(|turn| turn.completed_ordinal),
        report,
        awaited_ordinal: recorded.awaited_ordinal,
        answer: last.clone(),
        state: ChildState::NoFailure,
        wake_error,
        failure_message,
        report_dir: recorded.report_dir,
    })
}

/// One child's entry in a parent notification. A handback is already within the
/// cap; a last message that stands in for one is not, so every output is
/// bounded here and marked when it was cut.
fn wait_agent_entry(
    id: &str,
    state: &str,
    output: Option<String>,
    finished: bool,
    progress: &ChildProgress,
    oracle: bool,
) -> serde_json::Value {
    let output = output.map(|output| {
        if oracle {
            mj_core::subagent::bounded_oracle_report(&output)
        } else {
            bounded_report(&output)
        }
    });
    let mut agent = serde_json::json!({
        "child_session_id":id,
        "report_source":report_source(state, &progress.report),
        "state":state,
        "finished":finished,
        "output":output.as_ref().map(|(output, _)| output),
        "report_dir":progress.report_dir,
    });
    if output.is_some_and(|(_, truncated)| truncated) {
        agent["truncated"] = serde_json::Value::Bool(true);
    }
    if oracle {
        agent["oracle"] = serde_json::Value::Bool(true);
    }
    if state == "failed" {
        match &progress.state {
            ChildState::Refused { profile_id } => {
                agent["failure"] = serde_json::json!({
                    "kind": "login_invalid",
                    "profile_id": profile_id,
                });
            }
            ChildState::Failed {
                reason,
                provider_failure: true,
                ..
            } => {
                agent["failure"] =
                    serde_json::json!({"kind": "provider_failure", "summary": reason});
            }
            _ => {}
        }
    }
    if let Some(messages) = &progress.unanswered_messages {
        let names = if messages.is_empty() {
            "Mailbox events from unknown senders".to_owned()
        } else {
            messages
                .iter()
                .map(|message| {
                    let sender = if message.sender.trim().is_empty() {
                        "Message from an unknown sender"
                    } else {
                        message.sender.as_str()
                    };
                    format!("{sender} ({})", message.key)
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut warning = format!("{names} got no response; the child may not have acted on them.");
        if messages.iter().any(|message| message.from_parent) {
            warning.push_str(" Resend your message if needed.");
        }
        if let Some(summary) = agent["failure"]["summary"].as_str() {
            agent["failure"]["summary"] = format!("{summary} {warning}").into();
        } else if agent["failure"].is_object() {
            agent["failure"]["summary"] = warning.into();
        } else {
            agent["failure"] = serde_json::json!({"kind":"mailbox_unanswered", "summary": warning});
        }
    }
    agent
}

/// The `facts` object of a `list_agents` entry: the lines of what the child's
/// worker observed when its last turn ended, rendered by the same code as the
/// mailbox report.
fn work_facts_entry(facts: &mj_core::work_facts::WorkFacts, now_ms: u64) -> serde_json::Value {
    serde_json::json!({
        "finish": {
            "observed_secs_ago": now_ms.saturating_sub(facts.observed_at_ms) / 1000,
            "lines": facts.render_lines(),
        }
    })
}

/// The facts a finished child's report carries: what its worker recorded in the
/// outcome that ended its last turn, or why there are none. A prompt outcome
/// without them is from a worker that predates the capture; an ending that was
/// not a completed prompt took no observation.
fn finish_facts(progress: &ChildProgress) -> mj_core::work_facts::WorkFacts {
    let Some(turn) = &progress.answer else {
        return mj_core::work_facts::WorkFacts::unavailable(
            "the child ended without a completed turn, so its worker did not observe it",
        );
    };
    turn.work_facts.clone().unwrap_or_else(|| {
        mj_core::work_facts::WorkFacts::unavailable(match turn.outcome {
            mj_core::state::TurnOutcomeKind::Completed { .. } => {
                "the child's worker predates this version"
            }
            _ => "the turn ended without completing, so its worker took no observation",
        })
    })
}

/// What the session list calls a child now, bounded to one line, or the task
/// name it was spawned with when the daemon holds no record. A person may
/// have renamed the child since it was spawned.
fn list_targets(
    projection: &RuntimeProjection,
    parent_id: &str,
    now: u64,
) -> Result<serde_json::Value> {
    let parent = projection
        .records
        .get(parent_id)
        .context("parent session disappeared")?;
    ensure!(
        parent
            .subagents
            .as_ref()
            .is_some_and(mj_core::subagent::SubagentPolicy::orchestrators_enabled)
            && !projection.subagents.contains_key(parent_id),
        "list_targets requires orchestrators enabled on a parent using Mjolnir delegation"
    );
    let metadata = &projection.metadata;
    let config = &metadata.config;
    let mut active_sessions = BTreeMap::new();
    for session in projection
        .records
        .values()
        .filter(|session| session.state.is_active())
    {
        *active_sessions
            .entry(session.target_template_id.as_str())
            .or_insert(0u64) += 1;
    }
    let gib = |bytes: u64| bytes as f64 / (1u64 << 30) as f64;
    let targets = config.enabled_targets().map(|(id, template)| {
        let host = super::snapshot::host_capacity_for_target(metadata, id);
        let reading = host.map(|host| &host.reading);
        let memory = reading.and_then(|reading| reading.host_memory());
        let sizing = super::snapshot::target_container_sizing(template, metadata, host);
        let default_container_size = sizing.as_ref().map(|sizing| serde_json::json!({
            "cpus": sizing.default.cpus,
            "memory_gib": gib(sizing.default.memory_bytes),
        }));
        let capacity = sizing.and_then(|sizing| sizing.capacity).map(|capacity| serde_json::json!({
            "committed_cpus": capacity.committed.cpus,
            "remaining_cpus": capacity.remaining.cpus,
            "committed_memory_gib": gib(capacity.committed.memory_bytes),
            "remaining_memory_gib": capacity.complete.then(|| gib(capacity.remaining.memory_bytes)),
        }));
        let recent = reading.filter(|reading| reading.has_reading).map(|reading| serde_json::json!({
            "sampled_age_seconds": reading.sampled_at_epoch_seconds.map(|sampled| now.saturating_sub(sampled)),
            "cpu_percent": reading.cpu_percent,
            "memory_used_gib": memory.map(|memory| gib(memory.used_bytes)),
            "memory_total_gib": memory.map(|memory| gib(memory.total_bytes)),
        }));
        let last_1h = host.and_then(|host| host.last_1h.as_ref()).map(|window| serde_json::json!({
            "window_seconds": window.window_seconds,
            "sample_count": window.sample_count,
            "cpu_percent_avg": window.cpu_percent_avg,
            "cpu_percent_max": window.cpu_percent_max,
            "memory_used_gib_avg": gib(window.memory_used_bytes_avg),
            "memory_used_gib_max": gib(window.memory_used_bytes_max),
        }));
        serde_json::json!({
            "target_id": id,
            "kind": template.kind_name(),
            "permissions": if template.is_yolo() { "yolo" } else { "guardian" },
            "is_current_target": id == parent.target_template_id,
            "host": {
                "machine_id": mj_core::config::target_machine_id(template, &config.machines),
                "ec2_instance_type": reading.and_then(|reading| reading.ec2_instance_type.as_ref()),
                "total_cpus": reading.filter(|reading| reading.has_reading).map(|reading| reading.logical_cores),
                "total_memory_gib": memory.map(|memory| gib(memory.total_bytes)),
            },
            "default_container_size": default_container_size,
            "active_sessions": active_sessions.get(id).copied().unwrap_or(0),
            "capacity": capacity,
            "usage": {
                "recent": recent,
                "last_1h": last_1h,
                "probe_error": reading.and_then(|reading| reading.probe_error.as_ref()),
            },
        })
    }).collect::<Vec<_>>();
    Ok(serde_json::json!({"targets": targets}))
}

fn listed_agent_role(
    relation: &mj_core::subagent::SubagentRecord,
    category: Option<&str>,
) -> &'static str {
    if relation.orchestrator || matches!(category, Some("supervisor" | "orchestrator")) {
        "orchestrator"
    } else if category == Some("subtle") {
        "subtle"
    } else {
        "straightforward"
    }
}

fn live_direct_children(projection: &RuntimeProjection) -> BTreeMap<&str, usize> {
    let mut counts = BTreeMap::new();
    for relation in projection.subagents.values() {
        // Use the same live-worker states that count against delegation slots.
        if relation.kind == mj_core::subagent::SubagentKind::Worker
            && projection
                .records
                .get(&relation.child_session_id)
                .is_some_and(|record| record.state.has_live_worker())
        {
            *counts
                .entry(relation.parent_session_id.as_str())
                .or_insert(0) += 1;
        }
    }
    counts
}

fn agent_repo_urls(
    record: Option<&mj_core::state::SessionRecord>,
    projection: Option<&RuntimeProjection>,
) -> Vec<String> {
    use mj_core::remote_git::{Shorthand, display_url, expand_github_source, parse_github_remote};
    record
        .filter(|record| record.project_directory.is_none() && record.managed_worktree.is_none())
        .and_then(|record| record.project_bundle(&projection?.metadata.config))
        .into_iter()
        .flat_map(|bundle| &bundle.repositories)
        .filter_map(|repository| repository.github.as_deref())
        .filter_map(|source| {
            if let Some(remote) = parse_github_remote(source, Shorthand::Allowed)
                && remote.is_default_endpoint()
            {
                return Some(format!(
                    "https://github.com/{}/{}",
                    remote.owner, remote.repository
                ));
            }
            expand_github_source(source)
                .ok()
                .map(|source| display_url(&source))
        })
        .collect()
}

fn live_agent_selection(
    projection: Option<&RuntimeProjection>,
    detail: &mj_controller::database::SubagentListingDetails,
    current_profile: Option<&str>,
) -> mj_core::storage::UsageSelection {
    let live = projection
        .and_then(|projection| projection.sessions.get(&detail.relation.child_session_id))
        .filter(|view| view.connected)
        .and_then(|view| view.operational.as_ref());
    let current = live
        .map(|operational| {
            mj_core::acp::EffectiveSessionConfig::from_operational(
                &operational.config,
                &operational.config_options,
            )
        })
        .unwrap_or_default();
    mj_core::storage::UsageSelection {
        model: current
            .model
            .or_else(|| detail.selection.model.clone())
            .or_else(|| {
                (current_profile == Some(detail.relation.profile_id.as_str()))
                    .then(|| detail.relation.model.clone())
                    .flatten()
            }),
        effort: current
            .effort
            .or_else(|| detail.selection.effort.clone())
            .or_else(|| {
                (current_profile == Some(detail.relation.profile_id.as_str()))
                    .then(|| detail.relation.effort.clone())
                    .flatten()
            }),
    }
}

fn live_child_name(record: Option<&mj_core::state::SessionRecord>, task_name: &str) -> String {
    record.map_or_else(
        || mj_core::state::bounded_one_line(task_name, mj_core::state::MESSAGE_NAME_CHARS),
        mj_core::state::SessionRecord::message_name,
    )
}

/// Say in a notification or `list_agents` entry that the child is parked: its turn
/// ended and its worker is stopped, so it holds no processes, and the next
/// `send_message` starts it again. Its state is reported as for any finished
/// child; only a parked child carries the field.
fn mark_parked(entry: &mut serde_json::Value, record: Option<&mj_core::state::SessionRecord>) {
    if record.is_some_and(|record| record.state == SessionState::Parked) {
        entry["parked"] = serde_json::Value::Bool(true);
    }
}

/// Say in a notification or `list_agents` entry that a child whose last turn
/// failed or was interrupted can still be given work: `send_message` runs it
/// again. [`SessionState::can_receive_child_work`] decides this for every
/// delivery, so this statement and a later `send_message` cannot disagree. A
/// child whose login the provider refused is repaired first, so it is not
/// marked.
fn mark_promptable(
    entry: &mut serde_json::Value,
    state: &str,
    progress: &ChildProgress,
    record: Option<&mj_core::state::SessionRecord>,
) {
    if matches!(state, "failed" | "interrupted")
        && matches!(
            progress.state,
            ChildState::Failed { .. } | ChildState::RetryableFailure { .. }
        )
        && record.is_some_and(|record| record.state.can_receive_child_work())
    {
        entry["promptable"] = serde_json::Value::Bool(true);
    }
}

/// Where a finished child's `output` came from: its handback, or the last
/// message of its turn when it handed nothing back. Unfinished and failed
/// children have no report.
fn report_source(state: &str, report: &ReportState) -> Option<&'static str> {
    (state == "completed").then_some(match report {
        ReportState::Delivered(_) => "handback",
        _ => "last_message",
    })
}

async fn load_startup_status(session_id: String) -> Result<Option<StartStatus>> {
    Ok(load_startup_context(session_id).await?.1)
}

async fn load_startup_context(session_id: String) -> Result<(Option<String>, Option<StartStatus>)> {
    // The daemon's writer publishes each session's latest group with its
    // other durable records, so a status read costs no store connection. A
    // `mj wait` reads this on every revision it wakes for. Only a process
    // without the writer reads the store.
    let status = match mj_controller::database::committed_state()? {
        Some(committed) => committed.startup_groups.get_shared(&session_id),
        None => {
            blocking("read durable startup status", move || {
                mj_controller::database::load_latest_startup_group(&session_id).map(|steps| {
                    mj_controller::database::StartupGroupStatus::of(&steps).map(Arc::new)
                })
            })
            .await?
        }
    };
    startup_context(status.as_deref())
}

fn startup_context(
    status: Option<&mj_controller::database::StartupGroupStatus>,
) -> Result<(Option<String>, Option<StartStatus>)> {
    let Some(status) = status else {
        return Ok((None, None));
    };
    let group_id = status.group_id.clone();
    if let Some(error) = &status.failure {
        return Ok((
            group_id,
            Some(StartStatus::Failed {
                message: error
                    .clone()
                    .unwrap_or_else(|| "session startup was rejected".to_owned()),
            }),
        ));
    }
    if status.pending {
        return Ok((group_id, Some(StartStatus::Pending)));
    }
    if let Some((api_prompt, accepted_ordinal)) = &status.last_done
        && *api_prompt
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error}"))?
    {
        return Ok((
            group_id,
            Some(StartStatus::Submitted {
                turn_id: accepted_ordinal
                    .context("completed startup prompt has no acceptance ordinal")?,
            }),
        ));
    }
    Ok((group_id, None))
}

async fn submit_prompt_with_id(
    handle: &SessionHandle,
    text: String,
    command_id: Option<String>,
) -> Result<u64> {
    handle
        .submit(
            match command_id {
                Some(id) => id,
                None => new_command_id("api")?,
            },
            RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new(text))],
            },
        )
        .await
}

/// Run one blocking database read off the async runtime, keeping its context.
async fn blocking<T, F>(label: &'static str, job: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    mj_core::runtime::spawn_blocking(job)
        .await
        .with_context(|| format!("{label} task panicked"))?
}

/// Run one blocking export step off the async runtime.
async fn export_blocking<T, F>(label: &'static str, job: F) -> Result<T, ExportError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    match mj_core::runtime::spawn_blocking(job).await {
        Ok(result) => result.map_err(ExportError::Failed),
        Err(error) => Err(ExportError::Failed(anyhow!(
            "{label} task panicked: {error}"
        ))),
    }
}

/// Where one session's work lives on its target.
async fn export_layout(session_id: String) -> Result<SessionExportLayout, ExportError> {
    export_blocking("resolve the session export layout", move || {
        let executor = CancellableProcessExecutor::with_timeout(EXPORT_TIMEOUT);
        Controller::load()?.session_export_layout(&session_id, &executor)
    })
    .await
}

/// The directory on the target the session's agent runs in.
///
/// It is the primary repository's directory for every target kind: a bundle
/// session's harness is launched in `<workspace root>/<primary destination>`,
/// and a bare project session's in its selected project directory, whose
/// parent is the layout's workspace root. A relative export path resolves
/// here so it means what it meant to the agent that wrote the file (#1079).
fn agent_working_directory(layout: &SessionExportLayout) -> Result<String, ExportError> {
    Ok(agent_working_directory_at(
        &layout.workspace_root,
        &primary_repository(layout)?.relative_destination,
    ))
}

/// The arguments of `worker diff` for a live session: the repository it works
/// in and the base to compare against.
fn session_diff_arguments(
    layout: &SessionExportLayout,
    options: crate::server::api::DiffOptions,
) -> Result<Vec<String>, ExportError> {
    let repository = agent_working_directory(layout)?;
    let mut arguments = vec!["diff".to_owned(), "--repository".to_owned(), repository];
    arguments.extend(session_base_arguments(layout, options.base));
    if options.json {
        arguments.push("--json".to_owned());
    }
    arguments.extend(
        options
            .paths
            .into_iter()
            .map(|path| format!("--path={path}")),
    );
    Ok(arguments)
}

/// The arguments naming the commit an export compares against: the caller's
/// own, else the session's recorded launch base.
fn session_base_arguments(layout: &SessionExportLayout, requested: Option<String>) -> Vec<String> {
    if let Some(base) = requested {
        return vec![format!("--base={base}")];
    }
    match &layout.managed_worktree {
        Some(worktree) => match &worktree.base_commit {
            Some(base) => vec!["--base".to_owned(), base.clone()],
            // Sessions created before the base was recorded still name
            // their branch, whose reflog says where it started.
            None => vec!["--branch".to_owned(), worktree.branch.clone()],
        },
        None => Vec::new(),
    }
}

fn primary_repository(
    layout: &SessionExportLayout,
) -> Result<&mj_checkpoint::checkpoint::CheckpointRepositorySpec, ExportError> {
    layout
        .repositories
        .iter()
        .find(|repository| repository.id == layout.primary_repository)
        .ok_or_else(|| {
            ExportError::Failed(anyhow!(
                "session workspace has no repository {:?}",
                layout.primary_repository
            ))
        })
}

/// The root a file export hands the target, and the path to look up inside it.
///
/// A path resolves in the directory the agent runs in, so a plain `secret.txt`
/// means the file the agent wrote (#1079). What bounds it depends on the
/// layout:
///
/// - With more than one repository, the others sit beside the primary one under
///   the workspace root, so `..` has to reach them. `../other/file` names what
///   `other/file` named when paths resolved at the workspace root, and the
///   workspace root is the boundary.
/// - With one repository there is no sibling to reach, and the workspace root
///   is not a boundary Hel owns: for a bare project session it is the parent
///   directory holding the user's other projects. The agent's own directory is
///   the boundary there, so `../other-project/.env` is refused.
///
/// The returned path carries no `..` of its own, so the target-side read keeps
/// both its own refusal of a path that tries to leave the root it is handed and
/// its canonicalizing check against symlinks out of that root. Nothing on the
/// target has to know about this, which keeps older installed workers working.
fn export_root_and_path(
    layout: &SessionExportLayout,
    relative: &Path,
) -> Result<(String, String), ExportError> {
    let primary = primary_repository(layout)?;
    let agent_directory = target_join(&layout.workspace_root, &primary.relative_destination);
    let (root, mut resolved) = if layout.repositories.len() > 1 {
        let prefix = primary
            .relative_destination
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect();
        (layout.workspace_root.clone(), prefix)
    } else {
        (agent_directory.clone(), Vec::new())
    };
    // Naming both only helps when they differ; for a single repository the
    // boundary is the directory the path resolved in.
    let climbed = || {
        if root == agent_directory {
            format!(
                "{} climbs above {agent_directory}, the directory the agent runs in",
                relative.display()
            )
        } else {
            format!(
                "{} climbs above the session workspace {root}; it was resolved in {agent_directory}",
                relative.display()
            )
        }
    };
    for component in relative.components() {
        match component {
            Component::Normal(part) => resolved.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir => {
                if resolved.pop().is_none() {
                    return Err(ExportError::Refused(climbed()));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(ExportError::Refused(format!(
                    "{} must be relative to {agent_directory}",
                    relative.display()
                )));
            }
        }
    }
    Ok((root, resolved.join("/")))
}

/// The worker `fetch` invocation for a path a caller asked to read.
///
/// A relative path resolves in the agent's directory, bounded as
/// [`export_root_and_path`] describes. An absolute path is taken as the target
/// spells it. Either way the target resolves symlinks and accepts the result
/// only inside the export root, the session's sub-agent report root, or the
/// target's temporary directory (#1327): the places a session's evidence lives.
fn fetch_arguments(
    layout: &SessionExportLayout,
    path: &Path,
    mode: FetchMode,
) -> std::result::Result<Vec<String>, ExportError> {
    // The path is target-side POSIX text whatever host the daemon runs on, so
    // classify it by its root component: on a Windows daemon a Linux target's
    // `/tmp/x` has no prefix and `is_absolute` would misfire, treating it as
    // relative and refusing it as a climb out of the boundary.
    let (root, target_path) = if path.has_root() {
        let (root, _) = export_root_and_path(layout, Path::new("."))?;
        (root, path.to_string_lossy().into_owned())
    } else {
        let (root, relative) = export_root_and_path(layout, path)?;
        let joined = target_join(&root, Path::new(&relative));
        (root, joined)
    };
    let mut arguments = vec![
        "fetch".to_owned(),
        "--path".to_owned(),
        target_path,
        "--root".to_owned(),
        root,
        "--root".to_owned(),
        layout.report_root.clone(),
        "--temp".to_owned(),
    ];
    if mode == FetchMode::Listing {
        arguments.push("--list".to_owned());
    }
    Ok(arguments)
}

struct CountingReader<R> {
    inner: R,
    bytes: u64,
}

impl<R: std::io::Read> std::io::Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let count = self.inner.read(buf)?;
        self.bytes = self.bytes.saturating_add(count as u64);
        Ok(count)
    }
}

async fn write_workspace_file(
    exports: Arc<dyn ExportRuntime>,
    session_id: String,
    path: PathBuf,
    body: UploadBody,
    expected_len: Option<u64>,
    overwrite: bool,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> Result<u64, ExportError> {
    use std::sync::atomic::Ordering;
    let session_admission = exports
        .workspace_admission(session_id.clone(), cancelled.clone())
        .await
        .map_err(|e| ExportError::Refused(format!("{e:#}")))?;
    let record = exports
        .session_record(&session_id)
        .ok_or_else(|| ExportError::Refused("unknown session".into()))?;
    let handle = exports
        .workspace_session(session_id.clone())
        .await
        .map_err(|e| ExportError::Refused(format!("{e:#}")))?;
    let layout = export_layout(session_id.clone()).await?;
    // An upload lands where a read of the same relative path finds it: resolved
    // in the directory the agent runs in, under the same boundary (#1079).
    let (root, relative) = export_root_and_path(&layout, &path)?;
    if cancelled.load(Ordering::Acquire) {
        return Err(ExportError::Refused("file upload cancelled".into()));
    }
    let mut lease = mj_controller::controller::IdleWorkspaceLease::acquire(
        &session_admission,
        &handle,
        record.harness_kind,
    )
    .await
    .map_err(|e| ExportError::Refused(format!("{e:#}")))?;
    if cancelled.load(Ordering::Acquire) {
        return Err(ExportError::Refused("file upload cancelled".into()));
    }
    let (reader, upload_abort) = body.into_parts();
    let worker_cancelled = cancelled.clone();
    let worker_abort = upload_abort.clone();
    let mut transfer = mj_core::runtime::spawn_blocking(move || {
        let binary = format!(
            "{}/hel",
            targets::worker_root(&layout.backend, &session_id)?
        );
        let mut argv = vec![
            binary,
            "worker".into(),
            "write-file".into(),
            "--root".into(),
            root,
            "--path".into(),
            relative,
        ];
        if let Some(len) = expected_len {
            argv.push("--length".into());
            argv.push(len.to_string());
        }
        if overwrite {
            argv.push("--overwrite".into());
        }
        let command = targets::command_on_locator(
            SshTrafficClass::Bulk,
            &layout.backend,
            &session_id,
            argv,
            "session file write",
        )?;
        let mut counting = CountingReader {
            inner: reader,
            bytes: 0,
        };
        let output = CancellableProcessExecutor::new(worker_cancelled)
            .execute_with_abortable_stdin(&command, &mut counting, worker_abort.as_ref())?;
        Ok::<_, anyhow::Error>((output, counting.bytes))
    });
    let (output, bytes_written) = loop {
        tokio::select! {
            output = &mut transfer => break output.map_err(|e| ExportError::Failed(e.into()))?.map_err(ExportError::Failed)?,
            () = tokio::time::sleep(Duration::from_millis(250)) => {
                if let Err(error) = tokio::time::timeout(Duration::from_secs(10), lease.verify()).await.context("checking file write barrier timed out").and_then(|r| r) {
                    cancelled.store(true, Ordering::Release);
                    upload_abort();
                    // Do not release ownership while a subprocess can still write.
                    match transfer.await {
                        Ok(Ok(_)) => {},
                        Ok(Err(failure)) => tracing::warn!("cancelled file transfer: {failure:#}"),
                        Err(failure) => tracing::warn!("file transfer task failed: {failure}"),
                    }
                    return Err(ExportError::Failed(error));
                }
            }
        }
    };
    let result = worker_output(output, "session file write").map(|_| bytes_written);
    lease.release().await.map_err(|e| {
        ExportError::Failed(
            e.context("file transfer ended but the workspace barrier could not be released"),
        )
    })?;
    result
}

/// Run one `hel worker ...` command on the session's target and return its
/// standard output.
async fn worker_command(
    layout: SessionExportLayout,
    session_id: String,
    arguments: Vec<String>,
    purpose: &'static str,
) -> Result<Vec<u8>, ExportError> {
    worker_command_with_input(layout, session_id, arguments, purpose, None).await
}

async fn worker_command_with_input(
    layout: SessionExportLayout,
    session_id: String,
    arguments: Vec<String>,
    purpose: &'static str,
    input: Option<Vec<u8>>,
) -> Result<Vec<u8>, ExportError> {
    worker_command_staging(
        layout,
        session_id,
        arguments,
        purpose,
        input,
        mj_controller::controller::ControlWorker,
    )
    .await
}

/// Whether a worker command must be retried through the staged control
/// worker because the root's own `hel` cannot run it.
///
/// An older `hel` answers clap's usage failure. A root whose `hel` is absent
/// answers 127 through a POSIX shell, SSH, or container exec; a local target
/// reports the failed lookup as a `NotFound` I/O error instead.
pub(super) fn should_stage_control_worker(
    locator: &targets::TargetLocator,
    result: &Result<CommandOutput>,
) -> bool {
    match result {
        Ok(output) => {
            output.status == CLAP_USAGE_EXIT_CODE || output.status == CONTROL_NOT_FOUND_EXIT_CODE
        }
        Err(error) => {
            matches!(locator, targets::TargetLocator::LocalBare { .. })
                && error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                })
        }
    }
}

/// Install a renewed GitHub App token on a session, through its live actor
/// when one is attached and directly in its worker root otherwise.
///
/// A session in `Error` keeps its target and worker root; an export such as a
/// branch push must still be able to authenticate there.
async fn install_renewed_session_github_token(
    sessions: &SessionControl,
    session_id: &str,
    layout: &SessionExportLayout,
    token: String,
) -> Result<(), ExportError> {
    if let Some(handle) = sessions
        .find_session(session_id.to_owned())
        .await
        .map_err(ExportError::Failed)?
    {
        return handle
            .install_github_token(token)
            .await
            .map_err(ExportError::Failed);
    }
    let root = targets::worker_root(&layout.backend, session_id).map_err(ExportError::Failed)?;
    let locator = layout.backend.clone();
    let cancellation = tokio_util::sync::CancellationToken::new();
    let _cancel = cancellation.clone().drop_guard();
    export_blocking("write the session GitHub token", move || {
        let executor = CancellableProcessExecutor::with_timeout(EXPORT_TIMEOUT)
            .with_cancellation(cancellation);
        mj_controller::controller::write_session_github_token(&token, &executor, &locator, &root)
    })
    .await
}

/// Run a worker command, staging `stager`'s control worker beside a root
/// whose own `hel` is too old to know the subcommand, or is missing.
async fn worker_command_staging(
    layout: SessionExportLayout,
    session_id: String,
    arguments: Vec<String>,
    purpose: &'static str,
    input: Option<Vec<u8>>,
    stager: impl targets::ControlWorkerStager + Send + 'static,
) -> Result<Vec<u8>, ExportError> {
    // Dropping this future (the client left, or the server is stopping)
    // cancels the target command instead of leaving it to run to its timeout.
    let token = tokio_util::sync::CancellationToken::new();
    let _cancel = token.clone().drop_guard();
    let output: CommandOutput = export_blocking(purpose, move || {
        let executor =
            CancellableProcessExecutor::with_timeout(EXPORT_TIMEOUT).with_cancellation(token);
        let run = |binary: &str| -> Result<CommandOutput> {
            let mut command =
                worker_command_spec_for(&layout, &session_id, binary, arguments.clone(), purpose)?;
            if let Some(input) = &input {
                command = command.with_sensitive_stdin(input.clone());
            }
            executor.execute(&command)
        };
        let first = run("hel");
        // A worker installed before this subcommand existed answers clap's
        // usage failure, and it stays until the session is next resumed. A
        // bare root whose `hel` is absent answers a command-not-found status,
        // or a local `NotFound` error. The current build's worker serves the
        // call from the control name beside it: staging it replaces nothing,
        // so no swap admission is needed.
        if !should_stage_control_worker(&layout.backend, &first)
            || executor.cancellation_requested()
        {
            return first;
        }
        let root = targets::worker_root(&layout.backend, &session_id)?;
        stager
            .stage_control_worker(&executor, &layout.backend, &session_id, &root)
            .with_context(|| format!("stage the current worker in {root} for the {purpose}"))?;
        run(targets::CONTROL_WORKER_NAME)
    })
    .await?;
    worker_output(output, purpose)
}

fn worker_command_spec(
    layout: SessionExportLayout,
    session: &str,
    arguments: Vec<String>,
    purpose: &'static str,
) -> Result<mj_core::targets::CommandSpec> {
    worker_command_spec_for(&layout, session, "hel", arguments, purpose)
}

/// `worker <arguments>` run by `binary`, a file in the session's worker root.
fn worker_command_spec_for(
    layout: &SessionExportLayout,
    session: &str,
    binary: &str,
    arguments: Vec<String>,
    purpose: &'static str,
) -> Result<mj_core::targets::CommandSpec> {
    let binary = format!(
        "{}/{binary}",
        targets::worker_root(&layout.backend, session)?
    );
    let mut argv = vec![binary, "worker".into()];
    argv.extend(arguments);
    targets::command_on_locator(
        SshTrafficClass::Bulk,
        &layout.backend,
        session,
        argv,
        purpose,
    )
}

fn worker_output(output: CommandOutput, purpose: &str) -> Result<Vec<u8>, ExportError> {
    match output.status {
        0 => Ok(output.stdout),
        // A worker installed before these subcommands existed answers clap's
        // usage failure. That is not a failed export: resuming the session
        // reinstalls the worker and the same call then works.
        CLAP_USAGE_EXIT_CODE => Err(ExportError::Refused(format!(
            "worker on this target does not support {purpose}; resume the session to upgrade"
        ))),
        // The worker met a precondition it could not satisfy — no recorded
        // base, no push remote, a path outside the workspace — and printed the
        // reason. That is the caller's to fix, so it is a refusal, not a
        // failed export.
        mj_checkpoint::archive::EXPORT_REFUSED_EXIT_CODE => Err(ExportError::Refused(
            refusal_reason(&output.stdout, &output.stderr, purpose),
        )),
        status => Err(ExportError::Failed(anyhow!(
            "{purpose} failed with status {status}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
}

/// The worker's own words for why it refused, or a plain statement when it
/// said nothing.
///
/// A refusing worker prints its reason on standard output, where nothing else
/// is written once it has refused. Standard error also carries the worker's
/// log, so reading the reason from there put a DEBUG line in front of it
/// whenever `RUST_LOG` was set (F-13). A worker from before that change
/// prints the reason only on standard error, which is still read then.
fn refusal_reason(stdout: &[u8], stderr: &[u8], purpose: &str) -> String {
    [stdout, stderr]
        .into_iter()
        .map(|stream| String::from_utf8_lossy(stream).trim().to_owned())
        .find(|reason| !reason.is_empty())
        .unwrap_or_else(|| format!("{purpose} was refused by the target"))
}

impl ApiBackend {
    fn viewer_projection(&self) -> Result<RuntimeProjection, ApiFailure> {
        self.exports
            .runtime_projection()
            .map_err(|error| {
                tracing::warn!(%error, "viewer could not read daemon publication");
                viewer_unavailable()
            })?
            .ok_or_else(viewer_unavailable)
    }

    async fn preflight_slot(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.preflight_slots
            .clone()
            .acquire_owned()
            .await
            .expect("preflight semaphore stays open")
    }
}

fn viewer_unavailable() -> ApiFailure {
    ApiFailure::unavailable("controller unavailable")
}

fn viewer_activity(label: &'static str) -> Result<mj_core::upgrade::Work, ApiFailure> {
    mj_controller::upgrade::activity(label).map_err(|_| viewer_unavailable())
}

fn join_viewer_task<T>(result: Result<T, tokio::task::JoinError>) -> Result<T, ApiFailure> {
    result.map_err(|error| {
        tracing::warn!(%error, "viewer request task failed");
        viewer_unavailable()
    })
}

async fn stored_viewer_work<T: Send + 'static>(
    label: &'static str,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<Result<T, String>, ApiFailure> {
    let admission = viewer_activity(label)?;
    Ok(super::flatten_stored(
        mj_core::runtime::spawn_blocking(move || {
            let _admission = admission;
            work()
        })
        .await,
    ))
}

async fn saved_project(
    created: &QuickBundleCreation,
) -> Result<mj_core::project_catalog::SavedProject> {
    let bundle_id = created.bundle_id.clone();
    let bundle = created
        .config
        .bundles
        .get(&bundle_id)
        .context("created project is missing")?
        .clone();
    let project = mj_core::runtime::spawn_blocking(move || {
        mj_controller::project_catalog::snapshot(
            &bundle,
            &CancellableProcessExecutor::with_timeout(Duration::from_secs(15)),
            false,
        )
    })
    .await??;
    Ok(mj_core::project_catalog::SavedProject {
        bundle_id,
        name: project.name(),
        project,
    })
}

fn bundle_failure(error: QuickBundleFailure) -> crate::server::BundleFailure {
    match error {
        QuickBundleFailure::InvalidSource(detail) => {
            tracing::debug!(error = %detail, "phone bundle source was invalid");
            Refusal::of(&detail).map_or(BundleFailure::InvalidSource, BundleFailure::Refused)
        }
        QuickBundleFailure::Persistence(detail) => {
            tracing::warn!(error = %detail, "phone bundle creation failed");
            Refusal::of(&detail).map_or(BundleFailure::Controller, BundleFailure::Refused)
        }
    }
}

impl SubagentBackend for ApiBackend {
    fn dashboard_backend(&self) -> Option<Arc<dyn crate::server::api::DashboardBackend>> {
        self.exports.clone().dashboard_backend()
    }

    /// The store's report plus the weekly quota each enabled quota-bearing
    /// profile consumed over its span. The quota poller's publications give
    /// the identities its history is stored under; nothing is cached here.
    fn usage_report(
        &self,
        query: mj_core::usage::SessionUsageQuery,
    ) -> BoxFuture<'_, Result<mj_core::usage::SessionUsageReport>> {
        Box::pin(async move {
            let config = self.profile_catalog.live()?;
            let profiles: Vec<_> = {
                let reports = self
                    .quota_reports
                    .lock()
                    .map_err(|_| anyhow!("profile quota reports lock poisoned"))?;
                config
                    .enabled_profiles()
                    .filter(|(id, _)| query.profile.as_deref().is_none_or(|wanted| wanted == *id))
                    .filter_map(|(id, _)| {
                        mj_controller::quota_telemetry::quota_usage_profile(id, reports.get(id))
                    })
                    .collect()
            };
            let models = mj_controller::profile_config::advertised_models();
            let projections: Vec<_> = self
                .exports
                .published_quota_projections()
                .into_iter()
                .filter(|p| {
                    query
                        .profile
                        .as_deref()
                        .is_none_or(|wanted| wanted == p.profile_id)
                })
                .collect();
            mj_core::runtime::spawn_blocking(move || {
                let mut report =
                    mj_controller::database::load_session_usage_report(&query, models)?;
                report.profile_providers = config
                    .profiles
                    .iter()
                    .map(|(id, profile)| {
                        (
                            id.clone(),
                            profile.quota_provider().display_name().to_owned(),
                        )
                    })
                    .collect();
                report.quota_projections = projections;
                // The store clipped the last selected interval to its snapshot time.
                report.quota_usage = mj_controller::quota_telemetry::weekly_quota_usage(
                    profiles,
                    &query.selected_intervals(report.until_ms),
                );
                Ok::<_, anyhow::Error>(report)
            })
            .await?
        })
    }

    fn create_bundle(
        &self,
        source: String,
        exact_sources: Option<Vec<String>>,
        discovered: bool,
        include_project: bool,
        admission: mj_core::upgrade::Work,
    ) -> BoxFuture<'_, Result<Result<CreateBundleResponse, BundleFailure>, ApiFailure>> {
        let exports = self.exports.clone();
        let slots = self.bundle_slots.clone();
        Box::pin(async move {
            // Once accepted, publication finishes even if the HTTP client disconnects.
            join_viewer_task(
                mj_core::runtime::spawn(async move {
                    let _admission = admission;
                    let _slot = slots
                        .acquire_owned()
                        .await
                        .expect("bundle semaphore stays open");
                    let created = exports
                        .create_viewer_bundle(source, exact_sources, discovered)
                        .await
                        .map_err(bundle_failure)?;
                    let project = if include_project {
                        Some(saved_project(&created).await.map_err(|error| {
                            tracing::warn!(%error, "created bundle snapshot failed");
                            BundleFailure::Controller
                        })?)
                    } else {
                        None
                    };
                    Ok(CreateBundleResponse {
                        bundle_id: created.bundle_id,
                        project,
                    })
                })
                .await,
            )
        })
    }

    fn preflight_new(
        &self,
        request: mj_client::daemon::NewSessionPreflightRequest,
        direct: bool,
        admission: Option<mj_core::upgrade::Work>,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'_, Result<Result<PreflightNew, PreflightFailure>, ApiFailure>> {
        let slots = self.preflight_slots.clone();
        Box::pin(async move {
            let config = self.viewer_projection()?.metadata.installed_config();
            let project_validation = request.project_directory.is_some();
            let work = move |cancelled| {
                let executor = CancellableProcessExecutor::new(cancelled)
                    .with_deadline(Duration::from_secs(30));
                super::preflight_new_request(config, request, executor).map_err(|error| {
                    if direct {
                        PreflightFailure::Terminal(format!("{error:#}"))
                    } else if error.downcast_ref::<super::InvalidNewMount>().is_some() {
                        PreflightFailure::InvalidMount
                    } else if project_validation {
                        PreflightFailure::Validation
                    } else {
                        PreflightFailure::InvalidRepository(format!("{error:#}"))
                    }
                })
            };
            let admitted = admission.is_some();
            let operation = async move {
                let slot = tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => return Err(viewer_unavailable()),
                    permit = slots.acquire_owned() => permit.expect("preflight semaphore stays open"),
                };
                let blocking = super::preflight::cancellable_blocking(slot, move |cancelled| {
                    let _admission = admission;
                    work(cancelled)
                });
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => Err(viewer_unavailable()),
                    answer = blocking => Ok(answer),
                }
            };
            // An admitted remote repair survives HTTP disconnect. Daemon
            // shutdown still cancels its process and the blocking closure
            // retains the upgrade permit until it exits.
            let answer = if admitted {
                join_viewer_task(mj_core::runtime::spawn(operation).await)??
            } else {
                operation.await?
            };
            Ok(answer.unwrap_or_else(|error| {
                tracing::warn!(%error, "phone preflight task failed");
                Err(PreflightFailure::Controller(format!(
                    "preflight task failed: {error}"
                )))
            }))
        })
    }

    fn preflight_resume(
        &self,
        session_id: String,
        target_id: String,
    ) -> BoxFuture<'_, Result<Result<PreflightResume, PreflightFailure>, ApiFailure>> {
        Box::pin(async move {
            let slot = self.preflight_slot().await;
            let publication = self.viewer_projection()?;
            let config = publication.metadata.installed_config();
            let state = mj_core::state::State {
                sessions: publication.records,
                environment_owners: publication.environment_owners,
                ..Default::default()
            };
            Ok(
                super::preflight::cancellable_blocking(slot, move |cancelled| {
                    super::run_resume_preflight(config, state, &session_id, &target_id, cancelled)
                })
                .await
                .map_err(|error| {
                    tracing::warn!(%error, "phone resume preflight task failed");
                    PreflightFailure::Controller(format!("resume preflight task failed: {error}"))
                }),
            )
        })
    }

    fn complete_path(
        &self,
        host: CompletionHost,
        prefix: String,
        kind: CompletionKind,
    ) -> BoxFuture<'_, Result<Result<PathCompletion, String>, ApiFailure>> {
        Box::pin(async move {
            let slot = self.preflight_slot().await;
            let config = self.viewer_projection()?.metadata.installed_config();
            Ok(
                super::preflight::cancellable_blocking(slot, move |cancelled| {
                    let executor = CancellableProcessExecutor::new(cancelled)
                        .with_deadline(Duration::from_secs(5));
                    super::config_only_controller(config)
                        .complete_path(&host, &prefix, kind, &executor)
                        .map_err(|error| format!("{error:#}"))
                })
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "phone path completion task failed");
                    Err(format!("path completion task failed: {error}"))
                }),
            )
        })
    }

    fn discover_projects(
        &self,
        request: mj_controller::project_picker::ProjectDiscoveryRequest,
    ) -> BoxFuture<
        '_,
        Result<Result<mj_controller::project_picker::ProjectDiscovery, &'static str>, ApiFailure>,
    > {
        Box::pin(async move {
            let slot = self.preflight_slot().await;
            Ok(super::project_discovery::discover(slot, request).await)
        })
    }

    fn project_catalog(
        &self,
        refresh: bool,
        retry: bool,
    ) -> BoxFuture<
        '_,
        Result<Result<mj_core::project_catalog::ProjectCatalogView, String>, ApiFailure>,
    > {
        Box::pin(async move {
            Ok(self
                .exports
                .clone()
                .project_catalog(refresh, retry)
                .await
                .map_err(|error| format!("{error:#}")))
        })
    }

    fn prepare_move(
        &self,
        selection: MoveSelection,
        direct: bool,
    ) -> BoxFuture<'_, Result<Result<MovePreparation, Option<Refusal>>, ApiFailure>> {
        let exports = self.exports.clone();
        Box::pin(async move {
            let admission = viewer_activity("move preparation")?;
            join_viewer_task(
                mj_core::runtime::spawn(async move {
                    let _admission = admission;
                    exports.prepare_move(selection).await.map_err(|error| {
                        tracing::debug!(%error, "move preparation rejected");
                        Refusal::of(&error)
                            .or_else(|| direct.then(|| Refusal::precondition(format!("{error:#}"))))
                    })
                })
                .await,
            )
        })
    }

    fn mark_conversation_read(
        &self,
        client_id: String,
        session_id: String,
        through: u64,
    ) -> BoxFuture<'_, Result<Result<(), String>, ApiFailure>> {
        Box::pin(async move {
            let Some(record) = self.exports.session_record(&session_id) else {
                return Ok(Err("unknown session".into()));
            };
            let admission = viewer_activity("read receipt")?;
            let result = mj_core::runtime::spawn_blocking(move || {
                let _admission = admission;
                mj_controller::database::persist_read_receipt(
                    &client_id,
                    &record.workspace_id,
                    &session_id,
                    through,
                )
            })
            .await;
            let answer = match result {
                Ok(result) => result.map(|_| ()).map_err(|error| format!("{error:#}")),
                Err(error) => Err(format!("phone read receipt task failed: {error}")),
            };
            if let Err(error) = &answer {
                tracing::warn!(%error, "could not persist a phone read receipt");
            }
            Ok(answer)
        })
    }

    fn client_state(
        &self,
        client_id: String,
        session_id: String,
    ) -> BoxFuture<'_, Result<Result<ViewerClientState, String>, ApiFailure>> {
        Box::pin(async move {
            let record = self.exports.session_record(&session_id);
            stored_viewer_work("viewer state read", move || {
                let workspace = record.context("unknown session")?.workspace_id;
                let state = mj_controller::database::client_session_state(
                    &client_id,
                    &workspace,
                    &session_id,
                )?;
                Ok(ViewerClientState {
                    draft: state.draft,
                    through_event_ordinal: state.through_event_ordinal,
                })
            })
            .await
        })
    }

    fn save_draft(
        &self,
        client_id: String,
        session_id: String,
        draft: String,
    ) -> BoxFuture<'_, Result<Result<(), String>, ApiFailure>> {
        Box::pin(async move {
            let record = self.exports.session_record(&session_id);
            stored_viewer_work("draft save", move || {
                let workspace = record.context("unknown session")?.workspace_id;
                mj_controller::database::persist_client_draft(
                    &client_id,
                    &workspace,
                    &session_id,
                    &draft,
                )
            })
            .await
        })
    }

    fn mark_workspace_read(
        &self,
        client_id: String,
        workspace_id: String,
    ) -> BoxFuture<'_, Result<Result<(), String>, ApiFailure>> {
        Box::pin(async move {
            let records = self.viewer_projection()?.records;
            stored_viewer_work("workspace read marker", move || {
                for session in records
                    .values()
                    .filter(|session| session.workspace_id == workspace_id)
                {
                    // A receipt can be behind the frontier this viewer already read.
                    mj_controller::database::persist_read_receipt(
                        &client_id,
                        &workspace_id,
                        &session.id,
                        session.viewed_through_event_ordinal,
                    )
                    .ok();
                }
                Ok(())
            })
            .await
        })
    }

    fn prompt_history(
        &self,
        session_id: String,
        query: String,
        scope: String,
    ) -> BoxFuture<'_, Result<Result<ViewerPromptHistory, String>, ApiFailure>> {
        Box::pin(async move {
            let record = self.exports.session_record(&session_id);
            stored_viewer_work("history search", move || {
                let bundle = record.context("unknown session")?.bundle_id;
                use mj_controller::database::{HistoryScope, search_prompt_page};
                let scope = match scope.as_str() {
                    "session" => HistoryScope::Session,
                    "all" => HistoryScope::All,
                    _ => HistoryScope::Project,
                };
                let found = search_prompt_page(
                    &session_id,
                    &bundle,
                    scope,
                    &query,
                    None,
                    MAX_HISTORY_MATCHES,
                )?;
                Ok(ViewerPromptHistory {
                    entries: found.entries.into_iter().map(|entry| entry.text).collect(),
                    truncated: found.next.is_some(),
                })
            })
            .await
        })
    }

    fn set_container_settings(
        &self,
        session_id: String,
        resource_allocation: Option<SessionResourceAllocation>,
        mounts: Vec<AdditionalMount>,
    ) -> BoxFuture<
        '_,
        Result<Result<Option<mj_core::state::ContainerUpgradePreview>, ActionOutcome>, ApiFailure>,
    > {
        let exports = self.exports.clone();
        Box::pin(async move {
            let reference =
                new_command_id("container-settings").map_err(|_| viewer_unavailable())?;
            let settings = ContainerSettingsChange {
                resource_allocation,
                mounts,
                mount_history: None,
            };
            match exports.set_container_settings(session_id, settings).await {
                Ok(preview) => Ok(Ok(preview)),
                Err(error) => {
                    if let Some(refusal) = Refusal::of(&error) {
                        if refusal.code() == Some(mj_core::upgrade::HANDOFF_REFUSAL_CODE) {
                            return Err(ApiFailure::handoff());
                        }
                        return Ok(Err(ActionOutcome::Refused(refusal)));
                    }
                    tracing::warn!(%reference, %error, "container settings request failed");
                    Ok(Err(ActionOutcome::Failed { reference }))
                }
            }
        })
    }

    fn container_upgrade_preview(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<Result<mj_core::state::ContainerUpgradePreview, String>, ApiFailure>>
    {
        Box::pin(async move {
            Ok(self
                .exports
                .clone()
                .preview_container_upgrade(session_id)
                .await
                .map_err(|error| format!("{error:#}")))
        })
    }

    fn resolve_suspend_blockers(
        &self,
        session_id: String,
    ) -> BoxFuture<
        '_,
        Result<
            Result<mj_core::suspend_consent::ResolveSuspendBlockersResponse, String>,
            ApiFailure,
        >,
    > {
        Box::pin(async move {
            Ok(self
                .exports
                .clone()
                .resolve_suspend_blockers(session_id)
                .await
                .map_err(|error| format!("{error:#}")))
        })
    }
    fn suspend_preflight(
        &self,
        session_id: String,
        dry_run: bool,
        without_checkpoint: bool,
        history_only: bool,
    ) -> BoxFuture<'_, Result<Result<mj_core::suspend_consent::SuspendPreflight, String>, ApiFailure>>
    {
        Box::pin(async move {
            Ok(self
                .exports
                .clone()
                .suspend_preflight(session_id, dry_run, without_checkpoint, history_only)
                .await
                .map_err(|error| format!("{error:#}")))
        })
    }

    fn purge_untracked(
        &self,
        request: PurgeUntrackedRequest,
    ) -> BoxFuture<'_, Result<Result<PurgeUntrackedResponse, PurgeUntrackedFailure>, ApiFailure>>
    {
        let exports = self.exports.clone();
        Box::pin(async move {
            let cancelled = Arc::default();
            let cancellation = super::ProcessCancellationGuard(Arc::clone(&cancelled));
            // The rewrite must finish observing cancellation before releasing session admission.
            let result = mj_core::runtime::spawn(exports.purge_untracked(request, cancelled)).await;
            drop(cancellation);
            Ok(result.unwrap_or_else(|error| {
                let message = if error.is_panic() {
                    let payload = error.into_panic();
                    let message = payload
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                        .unwrap_or("unknown panic");
                    format!("purge untracked panicked: {message}")
                } else {
                    return Err(PurgeUntrackedFailure::Unavailable {
                        message: "the controller is unavailable".into(),
                        handoff: false,
                    });
                };
                Err(PurgeUntrackedFailure::Internal(message))
            }))
        })
    }

    fn stop_background_task(
        &self,
        session_id: String,
        background_task_id: String,
    ) -> BoxFuture<'_, Result<Result<(), BackgroundTaskStopFailure>, ApiFailure>> {
        let exports = self.exports.clone();
        Box::pin(async move {
            let admission = viewer_activity("background task stop")?;
            // Keep the upgrade activity through the worker's acknowledgement.
            join_viewer_task(
                mj_core::runtime::spawn(async move {
                    let _admission = admission;
                    exports
                        .stop_viewer_background_task(session_id, background_task_id)
                        .await
                })
                .await,
            )
        })
    }

    fn push_notice(&self, session_id: &str, message: String) {
        self.exports.push_notice(session_id, message);
    }
    fn subagent_work_liveness(
        &self,
        child_session_id: String,
    ) -> BoxFuture<'_, mj_core::subagent::Liveness> {
        Box::pin(async move {
            // A lookup that fails says nothing about the child's processes.
            self.live_work(&child_session_id)
                .await
                .unwrap_or(mj_core::subagent::Liveness::Unknown)
        })
    }

    fn profile_login_errors(&self) -> Result<BTreeMap<String, String>> {
        Ok(self
            .quota_reports
            .lock()
            .map_err(|_| anyhow!("profile quota reports lock poisoned"))?
            .iter()
            .filter_map(|(id, publication)| {
                publication
                    .login_error
                    .clone()
                    .map(|error| (id.clone(), error))
            })
            .collect())
    }

    fn subagent_config(&self) -> BoxFuture<'_, Result<mj_core::config::SubagentConfig>> {
        Box::pin(async move { Ok(self.exports.subagent_config()) })
    }

    fn quota_telemetry(
        &self,
    ) -> BoxFuture<'_, Result<mj_controller::quota_telemetry::QuotaTelemetrySnapshot>> {
        Box::pin(async move { Ok(self.exports.quota_telemetry()) })
    }
    fn quota_telemetry_store(&self) -> mj_controller::quota_telemetry::QuotaTelemetryStore {
        self.exports.quota_telemetry_store()
    }

    fn power_ranking(
        &self,
    ) -> BoxFuture<'_, Result<mj_controller::power_ranking::PowerRankingSnapshot>> {
        Box::pin(async move { Ok(self.exports.power_ranking()) })
    }

    fn record_placement(&self, profile_id: &str, category: &str) {
        self.exports.record_placement(profile_id, category);
    }

    fn wait_revision(&self, session_id: &str) -> Result<Option<u64>> {
        Ok(
            mj_controller::database::committed_state()?.map(|committed| {
                committed
                    .wait_revisions
                    .get(session_id)
                    .copied()
                    .unwrap_or(0)
            }),
        )
    }

    fn validate_github_bundle(
        &self,
        bundle_id: String,
    ) -> BoxFuture<'_, Result<(), mj_controller::controller::GithubBundleSelectionError>> {
        Box::pin(async move {
            let config = mj_core::runtime::spawn_blocking(mj_core::config::Config::load)
                .await
                .map_err(|error| {
                    mj_controller::controller::GithubBundleSelectionError::Provider(anyhow!(
                        "configuration load task failed: {error}"
                    ))
                })?
                .map_err(mj_controller::controller::GithubBundleSelectionError::Provider)?;
            mj_controller::controller::config_only_controller(config)
                .validate_github_bundle_installations(&bundle_id)
                .await
        })
    }

    fn github_token(
        &self,
        owner: Option<String>,
        repositories: Vec<(String, String)>,
    ) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            let config = mj_core::runtime::spawn_blocking(mj_core::config::Config::load)
                .await
                .context("GitHub token configuration task failed")??;
            let app = config.github.app.as_ref().ok_or_else(|| {
                anyhow!(
                    "GitHub App credentials are not configured; set [github.app] in config.toml"
                )
            })?;
            let provider = mj_controller::controller::GithubAppTokenProvider::shared(app)?;
            match owner {
                Some(owner) if repositories.is_empty() => provider.token_for_owner(&owner).await,
                Some(_) => anyhow::bail!("choose an owner or repositories, not both"),
                None => provider.token_for_repositories(&repositories).await,
            }
        })
    }

    fn subagent_report(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<Option<(bool, mj_core::subagent::SubagentReport)>>> {
        Box::pin(async move {
            // As for startup status: the daemon answers from its published
            // records, and only a process without the writer reads the store.
            if let Some(committed) = mj_controller::database::committed_state()? {
                return Ok(committed.state.subagents.get(&session_id).map(|record| {
                    (
                        record.handback_tool,
                        committed
                            .subagent_reports
                            .get(&session_id)
                            .cloned()
                            .unwrap_or_default(),
                    )
                }));
            }
            blocking("load sub-agent report", move || {
                let Some(record) = mj_controller::database::load_subagent(&session_id)? else {
                    return Ok(None);
                };
                let report = mj_controller::database::load_subagent_report(&session_id)?;
                Ok(Some((record.handback_tool, report)))
            })
            .await
        })
    }

    /// Create the workspace, then republish the list so the terminal tabs and
    /// the viewer see it without waiting for the next daemon action.
    fn create_workspace(
        &self,
        name: String,
    ) -> BoxFuture<'_, Result<mj_core::workspace::WorkspaceRecord>> {
        Box::pin(async move {
            let workspace = mj_core::runtime::spawn_blocking(move || {
                mj_controller::database::create_or_get_workspace(&name)
            })
            .await??;
            self.exports.refresh_workspaces().await?;
            Ok(workspace)
        })
    }

    fn refresh_workspaces(&self) -> BoxFuture<'_, Result<()>> {
        self.exports.refresh_workspaces()
    }

    fn close_workspace(
        &self,
        id: String,
        admission: mj_core::upgrade::Work,
    ) -> BoxFuture<'_, std::result::Result<(), ApiFailure>> {
        Box::pin(async move {
            self.exports
                .clone()
                .close_workspace(id, admission)
                .await
                .map_err(|error| ApiFailure::conflict(format!("{error:#}")))
        })
    }

    fn cancel_workspace_close(&self, id: &str) -> std::result::Result<(), ApiFailure> {
        self.exports
            .cancel_workspace_close(id)
            .map_err(|error| ApiFailure::conflict(format!("{error:#}")))
    }

    fn list_workspace_drafts(
        &self,
    ) -> BoxFuture<'_, Result<Vec<mj_client::daemon::WorkspaceDrafts>>> {
        self.exports.list_workspace_drafts()
    }

    fn remove_project(&self, id: String) -> BoxFuture<'_, Result<()>> {
        self.exports.remove_project(id)
    }

    fn profile_config(
        &self,
        profile: String,
        model: Option<String>,
        refresh: bool,
    ) -> BoxFuture<'_, Result<mj_core::worker_launch::ProfileConfig>> {
        Box::pin(async move {
            match model {
                Some(model) if !refresh => {
                    self.profile_catalog
                        .model_capabilities(profile, model)
                        .await
                }
                None if !refresh => Ok(self
                    .profile_catalog
                    .capabilities(&[profile])
                    .await?
                    .remove(0)),
                model => {
                    // A forced refresh bypasses the catalogue's cache, not its
                    // allow-list: an explicit request cannot select a model the
                    // picker hides.
                    let (mut choices, default) = refreshed_profile_choices(model, |model| {
                        mj_controller::controller::profile_config::discover(
                            profile.clone(),
                            model,
                            refresh,
                        )
                    })
                    .await?;
                    let selected = choices.model.clone();
                    choices.model = default;
                    let offer = self.profile_catalog.model_offer(&profile);
                    choices = choices.restricted_to(&offer);
                    choices.model = selected;
                    Ok(choices)
                }
            }
        })
    }

    fn unrestricted_profile_config(
        &self,
        profile: String,
    ) -> BoxFuture<'_, Result<mj_core::worker_launch::ProfileConfig>> {
        Box::pin(async move {
            self.profile_catalog
                .unrestricted_capabilities(&profile)
                .await
        })
    }

    fn warm_profile_capabilities(&self, config: &mj_core::config::Config) {
        self.profile_catalog.ensure(config);
    }

    fn profile_capabilities_snapshot(
        &self,
    ) -> Option<mj_core::profile_capabilities::ProfileCapabilitiesSnapshot> {
        Some(self.profile_catalog.snapshot().into_inner())
    }

    fn subagent_candidates(
        &self,
        _parent_profile: String,
    ) -> BoxFuture<'_, Result<crate::server::api::SubagentCandidates>> {
        Box::pin(async move {
            let ids = self.profile_catalog.candidates()?;
            self.discover_profile_candidates(ids, "sub-agent").await
        })
    }

    fn reviewer_candidates(&self) -> BoxFuture<'_, Result<crate::server::api::SubagentCandidates>> {
        Box::pin(ApiBackend::reviewer_candidates(self))
    }

    fn reviewer_candidates_for_model(
        &self,
        model: String,
        tried: Vec<String>,
    ) -> BoxFuture<'_, Result<crate::server::api::SubagentCandidates>> {
        Box::pin(async move {
            use futures::StreamExt;
            let mut ids = self.profile_catalog.candidates()?;
            ids.retain(|(id, _)| !tried.contains(id));
            let config = self.profile_catalog.live()?;
            // Refused logins and missing login files fail at launch, so they
            // are reported instead of probed.
            let (ids, mut unavailable) = self.filter_eligible_profiles(ids, &config).await?;
            let mut pending = futures::stream::FuturesUnordered::new();
            for (id, _) in ids {
                let catalog = &self.profile_catalog;
                pending.push(async move {
                    let result = catalog.capabilities(std::slice::from_ref(&id)).await;
                    (id, result)
                });
            }
            let mut checked_without_match = false;
            while let Some((id, result)) = pending.next().await {
                match result {
                    Ok(choices)
                        if choices.iter().any(|choice| {
                            mj_core::model_catalog::resolve(&choice.models, &model).is_some()
                        }) =>
                    {
                        drop(pending);
                        let mut candidates = self.reviewer_ready_candidates(model).await?;
                        candidates
                            .offered
                            .retain(|candidate| !tried.contains(&candidate.profile_id));
                        candidates.unavailable.retain(|(id, _)| !tried.contains(id));
                        return Ok(candidates);
                    }
                    Ok(choices) if choices.is_empty() => {
                        unavailable.push((id, "its discovery returned nothing".to_owned()));
                    }
                    Ok(_) => checked_without_match = true,
                    Err(error) => {
                        tracing::warn!(
                            profile_id = %id,
                            %error,
                            "reviewer profile left out of model selection"
                        );
                        unavailable.push((id, format!("{error:#}")));
                    }
                }
            }
            if unavailable.is_empty() {
                anyhow::bail!("no further enabled reviewer profile offers model {model:?}");
            }
            if tried.is_empty() && !checked_without_match {
                anyhow::bail!(
                    "no reviewer profile is available: {}",
                    format_unavailable_reasons(&unavailable)
                );
            }
            Ok(crate::server::api::SubagentCandidates {
                offered: Vec::new(),
                unavailable,
            })
        })
    }

    fn reviewer_autoconfigure_candidates(
        &self,
    ) -> BoxFuture<'_, Result<crate::server::api::SubagentCandidates>> {
        Box::pin(async move {
            use futures::StreamExt;
            let ids = self.profile_catalog.candidates()?;
            let config = self.profile_catalog.live()?;
            let (ids, mut unavailable) = self.filter_eligible_profiles(ids, &config).await?;
            let mut pending = futures::stream::FuturesUnordered::new();
            for (id, _) in &ids {
                let id = id.clone();
                let catalog = &self.profile_catalog;
                pending.push(async move {
                    let result = catalog.capabilities(std::slice::from_ref(&id)).await;
                    (id, result)
                });
            }
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
            let mut answered = Vec::new();
            let mut failed = Vec::new();
            let mut models = Vec::new();
            let mut timed_out = false;
            loop {
                let (id, result) = match tokio::time::timeout_at(deadline, pending.next()).await {
                    Ok(Some(item)) => item,
                    Ok(None) => break,
                    Err(_) => {
                        timed_out = true;
                        break;
                    }
                };
                match result {
                    Ok(choices) if choices.is_empty() => {
                        failed.push(id.clone());
                        unavailable.push((id, "its discovery returned nothing".to_owned()));
                    }
                    Ok(choices) => {
                        for choice in choices {
                            models.extend(choice.models);
                        }
                        answered.push(id);
                        if mj_core::review::autoconfigure::autoconfigure(
                            &mj_core::review::autoconfigure::merged_model_offers(&models),
                            |_| None,
                        )
                        .is_some()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            profile_id = %id,
                            %error,
                            "reviewer profile left out of autoconfigure"
                        );
                        failed.push(id.clone());
                        unavailable.push((id, format!("{error:#}")));
                    }
                }
            }
            drop(pending);
            if timed_out {
                for (id, _) in &ids {
                    if !answered.contains(id) && !failed.contains(id) {
                        unavailable.push((id.clone(), "capability discovery timed out".to_owned()));
                    }
                }
            }
            if answered.is_empty() && !unavailable.is_empty() {
                anyhow::bail!(
                    "no reviewer profile is available: {}",
                    format_unavailable_reasons(&unavailable)
                );
            }
            let mut candidates = self
                .discover_profile_candidates(
                    ids.into_iter()
                        .filter(|(id, _)| answered.contains(id))
                        .collect(),
                    "reviewer",
                )
                .await?;
            for (id, reason) in unavailable {
                if !candidates.unavailable.iter().any(|(known, _)| *known == id) {
                    candidates.unavailable.push((id, reason));
                }
            }
            Ok(candidates)
        })
    }

    fn cached_model_efforts(
        &self,
        profile: &str,
        model: &str,
    ) -> Option<Vec<mj_core::acp::SessionConfigChoice>> {
        self.profile_catalog.cached_model_efforts(profile, model)
    }

    fn reviewer_ready_candidates(
        &self,
        _model: String,
    ) -> BoxFuture<'_, Result<crate::server::api::SubagentCandidates>> {
        Box::pin(async move {
            // Remove refused logins and missing native login files before the
            // shared selector resolves the requested alias once over the
            // remaining ready profiles.
            let ids = self.profile_catalog.ready_candidates("")?;
            let config = self.profile_catalog.live()?;
            let (ids, unavailable) = self.filter_eligible_profiles(ids, &config).await?;
            let snapshot = self.profile_catalog.snapshot();
            let quota_reports = self
                .quota_reports
                .lock()
                .map_err(|_| anyhow!("profile quota reports lock poisoned"))?;
            let health = mj_controller::profile_health::shared()
                .lock()
                .map_err(|_| anyhow!("profile health lock poisoned"))?;
            let now = std::time::SystemTime::now();
            let now_seconds = i64::try_from(mj_core::clock::epoch_seconds()).unwrap_or(i64::MAX);
            let mut candidates = crate::server::api::SubagentCandidates {
                unavailable,
                ..Default::default()
            };
            for (profile_id, harness) in ids {
                let Some(profile) = config.enabled_profile(&profile_id) else {
                    continue;
                };
                let Some(entry) = snapshot
                    .profiles
                    .get(&profile.capabilities_key(&profile_id))
                else {
                    continue;
                };
                let mj_core::profile_capabilities::CapabilityState::Ready(choices) = &entry.choices
                else {
                    continue;
                };
                let unhealthy = choices
                    .models
                    .iter()
                    .filter_map(|model| {
                        health
                            .unhealthy(&profile_id, &model.value, now)
                            .cloned()
                            .map(|mark| (model.value.clone(), mark))
                    })
                    .collect();
                let publication = quota_reports.get(&profile_id);
                candidates
                    .offered
                    .push(crate::server::api::SubagentCandidate {
                        profile_id: profile_id.clone(),
                        harness,
                        choices: choices.clone(),
                        quota: crate::server::api::CandidateQuota::from_publication(
                            publication,
                            harness,
                            now_seconds,
                        ),
                        may_consume_credits: profile.load_balancing_may_consume_credits,
                        unhealthy,
                        cache_identity: publication.map(|p| p.cache_identity.clone()),
                    });
            }
            Ok(candidates)
        })
    }

    fn session_profile_candidates(
        &self,
    ) -> BoxFuture<'_, Result<crate::server::api::SubagentCandidates>> {
        Box::pin(async move {
            let ids = self.profile_catalog.configured_candidates()?;
            self.discover_profile_candidates(ids, "configured").await
        })
    }

    fn session_working_context(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<Option<mj_client::http::SessionWorkingContext>>> {
        self.exports.session_working_context(session_id)
    }

    fn session_git_status(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<Option<mj_core::local_git::SessionGitStatus>>> {
        self.exports.session_git_status(session_id)
    }

    fn wiki_sync_request(&self) -> Result<mj_client::daemon::WikiSyncStatus> {
        self.exports.wiki_sync_request()
    }

    fn wiki_sync_watch(
        &self,
        epoch: String,
        after: u64,
    ) -> BoxFuture<'_, Result<mj_client::daemon::WikiSyncStatus>> {
        self.exports.wiki_sync_watch(epoch, after)
    }

    fn wiki_search(&self, query: String, limit: usize) -> BoxFuture<'_, Result<WikiSearchPage>> {
        let runtime = Arc::clone(&self.exports);
        Box::pin(async move { runtime.wiki_search(query, limit).await })
    }

    fn request_ui(
        &self,
        body: mj_client::ui_request::UiRequestBody,
    ) -> std::result::Result<
        mj_client::ui_request::UiRequestReceipt,
        mj_client::ui_request::UiRequestRefusal,
    > {
        self.exports.request_ui(body)
    }

    fn wiki_brief(
        &self,
        wiki_id: String,
        max_chars: usize,
    ) -> BoxFuture<'_, Result<Option<String>>> {
        let runtime = Arc::clone(&self.exports);
        Box::pin(async move { runtime.wiki_brief(wiki_id, max_chars).await })
    }

    fn wiki_hits(
        &self,
        wiki_id: String,
        query: String,
        context_messages: usize,
        per_message_chars: usize,
    ) -> BoxFuture<'_, Result<Option<WikiHitTranscript>>> {
        let runtime = Arc::clone(&self.exports);
        Box::pin(async move {
            runtime
                .wiki_hits(wiki_id, query, context_messages, per_message_chars)
                .await
        })
    }

    fn wiki_session(&self, wiki_id: String) -> BoxFuture<'_, Result<Option<WikiSessionInfo>>> {
        let runtime = Arc::clone(&self.exports);
        Box::pin(async move { runtime.wiki_session(wiki_id).await })
    }

    fn wiki_restore(
        &self,
        request: WikiRestoreRequest,
    ) -> BoxFuture<'_, Result<Option<mj_client::daemon::RegisteredSession>>> {
        let runtime = Arc::clone(&self.exports);
        Box::pin(async move { runtime.wiki_restore(request).await })
    }

    fn start_subagent(
        &self,
        request: mj_controller::controller::RegisterSubagentRequest,
    ) -> BoxFuture<'_, Result<mj_core::subagent::SubagentRecord>> {
        let runtime = Arc::clone(&self.exports);
        Box::pin(async move { runtime.spawn_subagent(request).await })
    }

    fn cancel_start(&self, session_id: String) -> BoxFuture<'_, Result<()>> {
        Arc::clone(&self.exports).cancel_startup(session_id)
    }

    fn set_config(
        &self,
        session_id: String,
        key: String,
        value: String,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let (group_id, status) = self.exports.startup_context(session_id.clone()).await?;
            ensure!(
                !matches!(status, Some(StartStatus::Pending)),
                "session initialization is still running"
            );
            let handle = self.sessions.session(session_id.clone()).await?;
            handle.set_config(key, value).await?;
            self.exports
                .dismiss_startup_status(session_id, group_id)
                .await?;
            Ok(())
        })
    }

    fn session_handle(&self, session_id: String) -> BoxFuture<'_, Result<Option<SessionHandle>>> {
        Box::pin(async move { Ok(self.sessions.session(session_id).await.ok()) })
    }

    fn prompt(&self, session_id: String, text: String) -> BoxFuture<'_, Result<u64>> {
        self.prompt_with_id(session_id, text, None)
    }

    fn prompt_with_id(
        &self,
        session_id: String,
        text: String,
        command_id: Option<String>,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let (group_id, status) = self.exports.startup_context(session_id.clone()).await?;
            ensure!(
                !matches!(status, Some(StartStatus::Pending)),
                "session initialization is still running"
            );
            let handle = self
                .sessions
                .session(session_id.clone())
                .await
                .with_context(|| format!("session {session_id} is not running"))?;
            let turn = submit_prompt_with_id(&handle, text, command_id)
                .await
                .map_err(super::prompt_submission_error)?;
            self.exports
                .dismiss_startup_status(session_id, group_id)
                .await?;
            Ok(turn)
        })
    }

    fn record_user_prompt(
        &self,
        session_id: String,
        command_id: String,
        text: String,
    ) -> BoxFuture<'_, ()> {
        Box::pin(super::actions::record_user_prompt(
            session_id, command_id, text,
        ))
    }

    fn resolve_message_target(&self, requested_id: String) -> BoxFuture<'_, Result<String>> {
        Box::pin(blocking("resolve the user's message target", move || {
            let state = Controller::load()?.state;
            subagent_input::resolve_session_target(&state, None, &requested_id)
        }))
    }

    fn deliver_message(
        &self,
        sender_session_id: String,
        target_session_id: String,
        text: String,
        request_id: String,
        created_at_ms: i64,
    ) -> BoxFuture<'_, Result<crate::server::api::SessionMessageResponse>> {
        Box::pin(async move {
            self.deliver_session_message(
                sender_session_id,
                target_session_id,
                text,
                request_id,
                created_at_ms,
            )
            .await
        })
    }

    fn turn_state(&self, session_id: String) -> BoxFuture<'_, Result<Option<TurnState>>> {
        Box::pin(async move {
            if let Some(committed) = mj_controller::database::committed_state()? {
                return Ok(committed.turns.get(&session_id).map(|turn| {
                    let (execution, active_turn, last_turn_outcome) = turn.state.clone();
                    TurnState {
                        execution,
                        active_turn,
                        last_turn_outcome,
                        queued_command_ids: turn.queued_command_ids.clone(),
                    }
                }));
            }
            blocking("load turn outcome", move || {
                Ok(
                    mj_controller::database::load_materialized_turn_with_queue(&session_id)?.map(
                        |((execution, active_turn, last_turn_outcome), queued_command_ids)| {
                            TurnState {
                                execution,
                                active_turn,
                                last_turn_outcome,
                                queued_command_ids,
                            }
                        },
                    ),
                )
            })
            .await
        })
    }

    fn turn_summary(
        &self,
        session_id: String,
        turn: TurnSpan,
    ) -> BoxFuture<'_, Result<TurnSummary>> {
        Box::pin(async move {
            blocking("load turn summary", move || {
                mj_controller::database::load_materialized_turn_summary(
                    &session_id,
                    turn.start_position,
                    turn.completed_position,
                )
            })
            .await
        })
    }

    fn start_followup(
        &self,
        session_id: String,
        followup: StartFollowup,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.start_followup_with_id(session_id, followup, new_command_id("api-startup")?)
                .await
        })
    }

    fn start_status(&self, session_id: String) -> BoxFuture<'_, Result<Option<StartStatus>>> {
        self.exports.startup_status(session_id)
    }

    fn transcript(
        &self,
        session_id: String,
        after_seq: u64,
        limit: usize,
        roles: Vec<mj_core::transcript::TranscriptRole>,
        finished_only: bool,
    ) -> BoxFuture<'_, Result<Option<TranscriptPage>>> {
        Box::pin(async move {
            blocking("load transcript page", move || {
                mj_controller::database::load_materialized_transcript_filtered(
                    &session_id,
                    after_seq,
                    limit,
                    roles,
                    finished_only,
                )
            })
            .await
        })
    }

    fn diff(
        &self,
        session_id: String,
        options: crate::server::api::DiffOptions,
    ) -> BoxFuture<'_, std::result::Result<String, ExportError>> {
        Box::pin(async move {
            self.require_live_target(&session_id)?;
            let layout = export_layout(session_id.clone()).await?;
            let arguments = session_diff_arguments(&layout, options)?;
            let stdout = worker_command(layout, session_id, arguments, "session diff").await?;
            String::from_utf8(stdout).map_err(|error| {
                ExportError::Failed(anyhow!("the session diff was not UTF-8: {error}"))
            })
        })
    }

    fn export_patch(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, std::result::Result<ExportPatch, ExportError>> {
        Box::pin(async move {
            let record = self
                .exports
                .session_record(&session_id)
                .ok_or_else(|| ExportError::Refused(format!("unknown session {session_id}")))?;
            if !mj_controller::controller::export_reads_checkpoint(&record) {
                let layout = export_layout(session_id.clone()).await?;
                let mut arguments =
                    session_diff_arguments(&layout, crate::server::api::DiffOptions::default())?;
                arguments.push("--patch".to_owned());
                let bytes = worker_command(layout, session_id, arguments, "session diff").await?;
                return Ok(ExportPatch {
                    bytes,
                    notice: None,
                });
            }
            let patch = export_from_checkpoint(record, |record, executor| {
                mj_controller::controller::checkpoint_patch(record, executor)
            })
            .await?;
            Ok(ExportPatch {
                bytes: patch.bytes,
                notice: patch.notice,
            })
        })
    }

    fn read_file(
        &self,
        session_id: String,
        path: PathBuf,
        mode: FetchMode,
    ) -> BoxFuture<'_, std::result::Result<FetchedPath, ExportError>> {
        Box::pin(async move {
            self.require_live_target(&session_id)?;
            fetch_transfer::read(session_id, path, mode).await
        })
    }

    fn read_context_file(
        &self,
        session_id: String,
        path: PathBuf,
    ) -> BoxFuture<'_, std::result::Result<Vec<u8>, ExportError>> {
        Box::pin(async move {
            self.require_live_target(&session_id)?;
            let layout = export_layout(session_id.clone()).await?;
            let root = agent_working_directory(&layout)?;
            let arguments = vec![
                "read-file".to_owned(),
                "--root".to_owned(),
                root,
                "--path".to_owned(),
                target_join("", &path).trim_start_matches('/').to_owned(),
            ];
            worker_command(layout, session_id, arguments, "sub-agent context file read").await
        })
    }

    fn write_file(
        &self,
        session_id: String,
        path: PathBuf,
        body: UploadBody,
        expected_len: Option<u64>,
        overwrite: bool,
    ) -> BoxFuture<'_, Result<u64, ExportError>> {
        let exports = self.exports.clone();
        Box::pin(async move {
            if matches!(
                self.start_status(session_id.clone())
                    .await
                    .map_err(ExportError::Failed)?,
                Some(StartStatus::Pending)
            ) {
                return Err(ExportError::Refused(
                    "session initialization is still running".into(),
                ));
            }
            // The supervised task owns the lease until the subprocess exits,
            // even if the request is cancelled while stdin is streaming.
            let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let _cancel_on_drop = super::ProcessCancellationGuard(cancelled.clone());
            let task = mj_core::runtime::spawn(write_workspace_file(
                exports,
                session_id,
                path,
                body,
                expected_len,
                overwrite,
                cancelled,
            ));
            mj_core::runtime::spawn(async move {
                let result = task
                    .await
                    .map_err(|error| ExportError::Failed(error.into()))
                    .and_then(|result| result);
                if let Err(error) = &result {
                    tracing::warn!(?error, "API file injection failed");
                }
                result
            })
            .await
            .map_err(|error| ExportError::Failed(error.into()))?
        })
    }

    fn push_branch(
        &self,
        session_id: String,
        branch: String,
    ) -> BoxFuture<'_, std::result::Result<PushedBranch, ExportError>> {
        Box::pin(async move {
            self.require_live_target(&session_id)?;
            self.require_idle_turn(&session_id).await?;
            let target = self
                .exports
                .session_record(&session_id)
                .and_then(|record| record.target);
            // Load the layout once; a session with no live actor still needs
            // its target to receive a renewed export token.
            let layout = export_layout(session_id.clone()).await?;
            if target.as_ref().is_some_and(|target| {
                !matches!(&**target, mj_core::state::TargetLocator::LocalBare { .. })
            }) && let Some(token) =
                mj_controller::controller::renew_session_github_app_token(session_id.clone())
                    .await
                    .map_err(ExportError::Failed)?
            {
                install_renewed_session_github_token(&self.sessions, &session_id, &layout, token)
                    .await?;
            }
            let repository = agent_working_directory(&layout)?;
            let arguments = vec![
                "push-branch".to_owned(),
                "--root".to_owned(),
                targets::worker_root(&layout.backend, &session_id).map_err(ExportError::Failed)?,
                "--repository".to_owned(),
                repository,
                "--branch".to_owned(),
                branch,
            ];
            let stdout =
                worker_command(layout, session_id, arguments, "session branch push").await?;
            let pushed: mj_checkpoint::archive::PushedBranch = serde_json::from_slice(&stdout)
                .map_err(|error| {
                    ExportError::Failed(anyhow!("the worker's push result was unreadable: {error}"))
                })?;
            Ok(PushedBranch {
                branch: pushed.branch,
                remote: pushed.remote,
            })
        })
    }

    fn bundle(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, std::result::Result<BundleExport, ExportError>> {
        Box::pin(async move {
            let record = self
                .exports
                .session_record(&session_id)
                .ok_or_else(|| ExportError::Refused(format!("unknown session {session_id}")))?;
            // A session with no checkout has only its stored checkpoint, which
            // is rebuilt into a scratch checkout and bundled from there.
            if mj_controller::controller::export_reads_checkpoint(&record) {
                return bundle_from_stored_checkpoint(record).await;
            }
            // A live session's committed history is in its checkout, so Git
            // writes the bundle there. No checkpoint is taken, so the export
            // neither waits for nor competes with the session's own lifecycle.
            let layout = export_layout(session_id.clone()).await?;
            let repository = primary_repository(&layout)?.id.clone();
            let mut arguments = vec![
                "bundle".to_owned(),
                "--repository".to_owned(),
                agent_working_directory(&layout)?,
            ];
            arguments.extend(session_base_arguments(&layout, None));
            let bytes = worker_command(layout, session_id, arguments, "session bundle").await?;
            Ok(BundleExport { repository, bytes })
        })
    }
}

/// Rebuild a stopped session's work from its stored checkpoint in a scratch
/// checkout, which can take minutes for a large repository. It is blocking
/// work that owns nothing the daemon must wait for at handoff, so it holds no
/// admission; dropping this future (the client left, or the server is
/// stopping) cancels its archive reads and Git processes.
async fn export_from_checkpoint<T: Send + 'static>(
    record: mj_core::state::SessionRecord,
    export: impl FnOnce(
        &mj_core::state::SessionRecord,
        &CancellableProcessExecutor,
    ) -> std::result::Result<T, mj_controller::controller::CheckpointExportError>
    + Send
    + 'static,
) -> std::result::Result<T, ExportError> {
    let token = tokio_util::sync::CancellationToken::new();
    let _cancel = token.clone().drop_guard();
    let exported = mj_core::runtime::spawn_blocking(move || {
        let executor = CancellableProcessExecutor::new(Default::default()).with_cancellation(token);
        export(&record, &executor)
    })
    .await
    .map_err(|error| {
        ExportError::Failed(anyhow!("the checkpoint export task panicked: {error}"))
    })?;
    exported.map_err(|error| match error {
        mj_controller::controller::CheckpointExportError::Refused(reason) => {
            ExportError::Refused(reason)
        }
        mj_controller::controller::CheckpointExportError::Failed(error) => {
            ExportError::Failed(error)
        }
    })
}

async fn bundle_from_stored_checkpoint(
    record: mj_core::state::SessionRecord,
) -> std::result::Result<BundleExport, ExportError> {
    let bundle = export_from_checkpoint(record, |record, executor| {
        mj_controller::controller::checkpoint_bundle(record, executor)
    })
    .await?;
    Ok(BundleExport {
        repository: bundle.repository,
        bytes: bundle.bytes,
    })
}

// Refresh the default before selecting a model, so the exemption belongs to
// this discovery rather than the catalogue from before the refresh.
async fn refreshed_profile_choices<F, Fut>(
    model: Option<String>,
    mut discover: F,
) -> Result<(mj_core::worker_launch::ProfileConfig, Option<String>)>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<mj_core::worker_launch::ProfileConfig>>,
{
    let choices = discover(None).await?;
    let default = choices.model.clone();
    let choices = match model {
        Some(model) => discover(Some(model)).await?,
        None => choices,
    };
    Ok((choices, default))
}

fn subagent_message_result(
    delivery: crate::server::api::SessionMessageResponse,
) -> Result<serde_json::Value> {
    let status = if matches!(delivery.via.as_str(), "mailbox" | "queued") {
        "queued"
    } else {
        "submitted"
    };
    let mut result = serde_json::json!({"status": status, "via": delivery.via});
    if delivery.managed_child {
        result["child_session_id"] = delivery.session_id.into();
        if let Some(turn_id) = delivery.turn_id {
            result["turn_id"] = turn_id.into();
        }
    } else {
        result["session_id"] = delivery.session_id.into();
        result["turn_id"] = serde_json::json!(delivery.turn_id);
    }
    if let Some(mode) = delivery.mailbox_delivery {
        result["mailbox_delivery"] = serde_json::to_value(mode)?;
    }
    if let Some(note) = delivery.note {
        result["note"] = note.into();
    }
    Ok(result)
}

#[cfg(test)]
mod tests;

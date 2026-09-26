//! The daemon half of the documented HTTP API.
//!
//! `server::api` serves the `/api/v1` routes against the
//! [`SubagentBackend`](crate::server::api::SubagentBackend) trait, so its
//! route tests can use a fake. This module is the daemon's implementation.
//!
//! Every database read runs on `spawn_blocking`: the web server's handlers run
//! on the async runtime, and a synchronous SQLite read on that thread would
//! stall unrelated requests.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
use anyhow::{Context, Result, anyhow, bail, ensure};

use mj_core::state::{MaterializedExecutionState, SessionState};
use mj_core::subagent::{DEFAULT_WAIT_SECONDS, ReportState, bounded_report};

use crate::quota::ProfileQuota;

use crate::controller::{Controller, SessionExportLayout};
use crate::server::api::{
    BundleExport, ExportError, PushedBranch, StartFollowup, StartStatus, SubagentBackend,
    TranscriptPage, TurnSpan, TurnState, TurnSummary,
};
use crate::targets::{self, CancellableProcessExecutor, CommandExecutor, CommandOutput};
use mj_client::session::{BoxFuture, SessionControl, SessionHandle, ViewError, new_command_id};
use mj_core::relay::RelayCommand;

mod subagent_input;

use crate::daemon::RuntimeState;
use mj_client::daemon::{WikiHitTranscript, WikiRestoreRequest, WikiSearchPage, WikiSessionInfo};

/// How the follow-up task learns whether a session is still on its way up.
///
/// It is a function rather than the daemon runtime itself because that is all
/// the follow-up needs, and a test can supply the states it wants to drive.
pub type SessionStateSource = Arc<dyn Fn(&str) -> Option<SessionState> + Send + Sync>;

/// What the export operations need from the daemon beyond a session's live
/// actor: the durable record, and a checkpoint on demand.
///
/// It is a trait rather than the daemon runtime itself so the backend can be
/// built in a test without one, the same reason the follow-up reads session
/// state through a function.
pub trait ExportRuntime: Send + Sync {
    /// The in-memory record for a session, or `None` when the daemon holds
    /// none.
    fn session_record(&self, session_id: &str) -> Option<mj_core::state::SessionRecord>;

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
    ) -> BoxFuture<'_, Result<crate::session_manager::ManagedSessionHandle>> {
        Box::pin(async { anyhow::bail!("workspace file injection is unavailable") })
    }

    /// Checkpoint a session now, returning the archive it wrote.
    fn checkpoint_now(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<mj_core::state::CheckpointMetadata>>;

    fn spawn_subagent(
        self: Arc<Self>,
        _request: crate::controller::RegisterSubagentRequest,
    ) -> BoxFuture<'static, Result<mj_core::subagent::SubagentRecord>> {
        Box::pin(async { anyhow::bail!("sub-agent creation is unavailable") })
    }

    fn close_subagent(self: Arc<Self>, _session_id: String) -> BoxFuture<'static, Result<()>> {
        Box::pin(async { anyhow::bail!("sub-agent close is unavailable") })
    }

    /// Whether a park of this child is still running. A backend without a
    /// daemon parks nothing.
    fn subagent_park_running(&self, _session_id: &str) -> bool {
        false
    }

    /// Stop an idle child's worker and keep everything else, once its parent
    /// has been told its turn ended (#1161).
    fn park_subagent(self: Arc<Self>, _session_id: String) -> BoxFuture<'static, Result<()>> {
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

    /// Whether the index is stale enough that a query should ask for a sync.
    fn wiki_sync_is_stale(&self) -> bool {
        false
    }

    fn wiki_request_sync(&self) {}

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
    ) -> BoxFuture<'static, Result<Option<String>>> {
        Box::pin(async { anyhow::bail!("SessionWiki restore is unavailable") })
    }
}

impl ExportRuntime for RuntimeState {
    fn session_record(&self, session_id: &str) -> Option<mj_core::state::SessionRecord> {
        RuntimeState::session_record(self, session_id)
    }

    fn close_is_requested(&self, session_id: &str) -> bool {
        RuntimeState::close_is_requested(self, session_id)
    }

    fn workspace_session(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<crate::session_manager::ManagedSessionHandle>> {
        Box::pin(async move { self.workspace_session_handle(&session_id).await })
    }

    fn checkpoint_now(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<mj_core::state::CheckpointMetadata>> {
        Box::pin(async move { self.checkpoint_session_now(&session_id).await })
    }

    fn spawn_subagent(
        self: Arc<Self>,
        request: crate::controller::RegisterSubagentRequest,
    ) -> BoxFuture<'static, Result<mj_core::subagent::SubagentRecord>> {
        Box::pin(async move { self.start_subagent_session(request).await })
    }

    fn close_subagent(self: Arc<Self>, session_id: String) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { self.suspend_session(session_id).await })
    }

    fn subagent_park_running(&self, session_id: &str) -> bool {
        RuntimeState::subagent_park_running(self, session_id)
    }

    fn park_subagent(self: Arc<Self>, session_id: String) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { RuntimeState::park_subagent(&self, session_id).await })
    }

    fn unpark_subagent(self: Arc<Self>, session_id: String) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { RuntimeState::unpark_subagent(&self, session_id).await })
    }

    fn refresh_workspaces(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(RuntimeState::refresh_workspaces(self))
    }

    fn wiki_sync_is_stale(&self) -> bool {
        crate::sessionwiki::sync_is_stale(self.wiki().last_success())
    }

    fn wiki_request_sync(&self) {
        self.wiki().request_sync(false);
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
    ) -> BoxFuture<'static, Result<Option<String>>> {
        Box::pin(async move {
            // The HTTP API has no request-scoped cancellation; the daemon's
            // shutdown drain cancels the queue's own token directly.
            Ok(self
                .restore_wiki_session(request, &tokio_util::sync::CancellationToken::new())
                .await?
                .map(|registered| registered.session.id))
        })
    }
}

/// How long the follow-up waits for a session to become usable before giving
/// up. Provisioning a container or an SSH host can take many minutes, and the
/// record state is what ends the wait early when the launch fails.
const START_DEADLINE: Duration = Duration::from_secs(30 * 60);
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
/// Clap's exit code for a usage failure, which is what a worker binary too old
/// to know the export subcommands answers.
const CLAP_USAGE_EXIT_CODE: i32 = 2;

/// How far one created session's follow-up has got, and the task driving it.
struct Start {
    status: StartStatus,
    /// Kept so the task is cancelled when the entry is pruned; a dropped
    /// handle would leave the task running against a session that is gone.
    task: Option<tokio::task::JoinHandle<()>>,
    cancel: tokio_util::sync::CancellationToken,
}

/// The backend the `/api/v1` routes drive sessions through.
pub struct ApiBackend {
    sessions: SessionControl,
    session_states: SessionStateSource,
    /// The daemon operations the export path needs: session records, and the
    /// checkpoint a bundle export is read from.
    exports: Arc<dyn ExportRuntime>,
    /// How far each created session's follow-up configuration and first prompt
    /// have got.
    starts: Arc<Mutex<BTreeMap<String, Start>>>,
    starts_changed: Arc<tokio::sync::Notify>,
    /// Latest background-refreshed quota reports, used to rank the profiles a
    /// sub-agent may run on so a child lands on the login with the most quota
    /// left, without making the parent reason about credential aliases.
    quota_reports: Arc<Mutex<BTreeMap<String, ProfileQuota>>>,
    /// Profiles whose login the credential sync found refused; a spawn on one
    /// is refused until its login file changes.
    rejected_logins: Arc<Mutex<mj_core::credentials::RejectedLogins>>,
    /// The capabilities `list_profiles` answers with and `spawn` chooses
    /// from. The catalogue discovers them in the background, so a call only
    /// ranks what it holds, waiting for a profile the pass has not published.
    profile_catalog: Arc<super::profile_catalog::ProfileCatalog>,
}

impl ApiBackend {
    pub fn new(
        sessions: SessionControl,
        session_states: SessionStateSource,
        exports: Arc<dyn ExportRuntime>,
    ) -> Self {
        Self {
            sessions,
            session_states,
            exports,
            starts: Arc::new(Mutex::new(BTreeMap::new())),
            starts_changed: Arc::default(),
            quota_reports: Arc::new(Mutex::new(BTreeMap::new())),
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

    pub fn with_quota_reports(
        mut self,
        quota_reports: Arc<Mutex<BTreeMap<String, ProfileQuota>>>,
    ) -> Self {
        self.quota_reports = quota_reports;
        self
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

    pub async fn execute_subagent_tool(
        self: &Arc<Self>,
        parent_session_id: String,
        request: mj_core::subagent::SubagentToolRequest,
    ) -> mj_core::subagent::SubagentToolResult {
        let outcome = self
            .execute_subagent_tool_inner(&parent_session_id, &request)
            .await;
        if let mj_core::subagent::SubagentToolAction::SendInput {
            child_session_id, ..
        } = &request.action
        {
            let (mut value, is_error) = match outcome {
                Ok(value) => (value, false),
                Err(error) => (
                    serde_json::json!({"status":"failed", "error":format!("{error:#}")}),
                    true,
                ),
            };
            value["child_session_id"] = child_session_id.clone().into();
            value["request_id"] = request.request_id.clone().into();
            value["created_at_ms"] = request.created_at_ms.into();
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
    ) -> Result<serde_json::Value> {
        use mj_core::subagent::SubagentToolAction;
        let request_created_at_ms = request.created_at_ms;
        match &request.action {
            SubagentToolAction::ListProfiles => {
                let parent = self
                    .exports
                    .session_record(parent_session_id)
                    .context("parent session disappeared")?;
                // The catalogue discovers every enabled profile in the
                // background, so this normally only ranks and merges what it
                // holds; a discovery still running is waited for.
                let candidates = self
                    .subagent_candidates(parent.last_profile.clone())
                    .await?;
                let profiles =
                    crate::server::api::merge_same_models(candidates.offered, &parent.last_profile)
                        .into_iter()
                        .map(|candidate| {
                            serde_json::json!({
                                "profile_id":candidate.profile_id,
                                "harness":candidate.harness.id(),
                                "default_model":candidate.choices.model,
                                "models":candidate.choices.models,
                                "efforts":candidate.choices.efforts,
                            })
                        })
                        .collect::<Vec<_>>();
                if !candidates.unavailable.is_empty() {
                    let unavailable = candidates
                        .unavailable
                        .into_iter()
                        .map(|(profile_id, reason)| {
                            serde_json::json!({"profile_id":profile_id,"reason":reason})
                        })
                        .collect::<Vec<_>>();
                    return Ok(serde_json::json!({"profiles":profiles,"unavailable":unavailable}));
                }
                Ok(serde_json::json!({"profiles":profiles}))
            }
            SubagentToolAction::Spawn {
                task_name,
                instructions,
                profile_id,
                model,
                effort,
                working_directory,
                context,
                files,
            } => {
                let parent = self
                    .exports
                    .session_record(parent_session_id)
                    .context("parent session disappeared")?;
                let backend: Arc<dyn crate::server::api::SubagentBackend> = self.clone();
                let selection = crate::server::api::resolve_subagent_selection(
                    &backend,
                    parent_session_id,
                    &parent.last_profile,
                    profile_id.as_deref(),
                    model.as_deref(),
                    effort.as_deref(),
                )
                .await
                .map_err(|failure| anyhow::anyhow!(failure.message))?;
                let ranges = files
                    .iter()
                    .flat_map(|entry| {
                        let file = entry.file.clone();
                        entry.ranges.iter().map(move |range| {
                            crate::server::api::SubagentSourceRange {
                                file: file.clone(),
                                start: range.start,
                                end: range.end,
                            }
                        })
                    })
                    .collect::<Vec<_>>();
                let prompt = crate::server::api::build_subagent_prompt(
                    &backend,
                    parent_session_id,
                    instructions,
                    context.as_deref(),
                    &ranges,
                )
                .await
                .map_err(|failure| anyhow::anyhow!(failure.message))?;
                let relation = self
                    .start_subagent(crate::controller::RegisterSubagentRequest {
                        parent_session_id: parent_session_id.to_owned(),
                        task_name: task_name.clone(),
                        profile_id: selection.profile_id,
                        model: Some(selection.model.clone()),
                        effort: selection.effort.clone(),
                        working_directory: working_directory.clone(),
                        initial_prompt: prompt,
                        request_key: request.request_id.clone(),
                        report_root: None,
                    })
                    .await?;
                // Registration completes the first prompt (it names the
                // handback tool when the child gets one), so send what it kept.
                self.start_followup(
                    relation.child_session_id.clone(),
                    crate::server::api::StartFollowup {
                        model: Some(selection.model),
                        effort: selection.effort,
                        prompt: Some(relation.initial_prompt.clone()),
                        fast_mode: selection.fast_mode,
                    },
                )
                .await?;
                let report_dir = blocking("load sub-agent report directory", {
                    let child_id = relation.child_session_id.clone();
                    move || crate::database::load_subagent_report(&child_id)
                })
                .await?
                .report_dir;
                Ok(serde_json::json!({
                    "child_session_id":relation.child_session_id,
                    "task_name":relation.task_name,
                    "profile_id":relation.profile_id,
                    "model":relation.model,
                    "report_dir":report_dir,
                }))
            }
            SubagentToolAction::ListAgents => {
                let inputs = self.subagent_input_progress(parent_session_id).await?;
                let relations = self.list_subagents(parent_session_id.to_owned()).await?;
                let child_ids = relations
                    .iter()
                    .map(|relation| relation.child_session_id.clone())
                    .collect::<Vec<_>>();
                let summaries = tokio::task::spawn_blocking(move || {
                    child_ids
                        .into_iter()
                        .map(|id| {
                            let summary = crate::database::load_materialized_session_summary(&id)?;
                            let progress = load_child_progress(&id)?;
                            Ok((id, (summary, progress)))
                        })
                        .collect::<Result<std::collections::BTreeMap<_, _>>>()
                })
                .await??;
                let mut starts = std::collections::BTreeMap::new();
                for relation in &relations {
                    if let Some(status) =
                        self.start_status(relation.child_session_id.clone()).await?
                    {
                        starts.insert(relation.child_session_id.clone(), status);
                    }
                }
                let agents = relations
                    .into_iter()
                    .map(|relation| {
                        let record = self.exports.session_record(&relation.child_session_id);
                        // This listing reports state only; a child's report is
                        // collected through wait.
                        let unknown = ChildProgress {
                            report: ReportState::Fallback,
                            awaited_ordinal: None,
                            answered_ordinal: None,
                            failed_turn: None,
                            login_failure: None,
                            report_dir: None,
                        };
                        let (summary, progress) = summaries
                            .get(&relation.child_session_id)
                            .map_or((None, &unknown), |(summary, progress)| {
                                (summary.as_ref(), progress)
                            });
                        let (state, _, _) = inputs.status(
                            &relation.child_session_id,
                            subagent_status(
                                record.as_ref(),
                                summary,
                                starts.get(&relation.child_session_id),
                                None,
                                self.exports.close_is_requested(&relation.child_session_id),
                                progress,
                            ),
                        );
                        let mut entry = serde_json::json!({
                            "child_session_id":relation.child_session_id,
                            "task_name":relation.task_name,
                            "profile_id":relation.profile_id,
                            "state":state,
                        });
                        inputs.annotate(&relation.child_session_id, &mut entry);
                        mark_parked(&mut entry, record.as_ref());
                        entry
                    })
                    .collect::<Vec<_>>();
                Ok(serde_json::json!({"agents":agents}))
            }
            SubagentToolAction::SendInput {
                child_session_id,
                message,
            } => {
                self.require_owned_child(parent_session_id, child_session_id)
                    .await?;
                let turn_id = self
                    .deliver_subagent_input(parent_session_id, child_session_id, message, request)
                    .await?;
                blocking("record sub-agent prompt", {
                    let child_id = child_session_id.clone();
                    move || crate::database::record_subagent_prompt(&child_id, turn_id)
                })
                .await?;
                Ok(
                    serde_json::json!({"child_session_id":child_session_id,"turn_id":turn_id,"status":"submitted"}),
                )
            }
            SubagentToolAction::WaitAgents {
                child_session_ids,
                timeout_seconds,
                return_when,
            } => {
                for child_id in child_session_ids {
                    self.require_owned_child(parent_session_id, child_id)
                        .await?;
                }
                // The budget runs from when the caller made the request, not
                // from when this daemon picked it up. A request that is
                // executed again — after a daemon restart, or after a result
                // could not be handed back — then still answers at the
                // caller's original deadline instead of starting over.
                let started = tokio::time::Instant::now();
                let remaining = mj_core::subagent::remaining_subagent_wait(
                    request_created_at_ms,
                    *timeout_seconds,
                    mj_core::clock::epoch_millis(),
                );
                tracing::info!(
                    parent_session_id,
                    children = child_session_ids.len(),
                    requested_seconds = timeout_seconds.unwrap_or(DEFAULT_WAIT_SECONDS),
                    remaining_seconds = remaining.as_secs(),
                    "starting a sub-agent wait"
                );
                let deadline = started + remaining;
                loop {
                    let inputs = self.subagent_input_progress(parent_session_id).await?;
                    let ids = child_session_ids.clone();
                    let summaries = tokio::task::spawn_blocking(move || {
                        ids.into_iter()
                            .map(|id| {
                                let summary =
                                    crate::database::load_materialized_session_summary(&id)?;
                                let progress = load_child_progress(&id)?;
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
                    let finished = summaries
                        .iter()
                        .map(|(id, summary, progress)| {
                            let record = self.exports.session_record(id);
                            inputs
                                .status(
                                    id,
                                    subagent_status(
                                        record.as_ref(),
                                        summary.as_ref(),
                                        starts.get(id),
                                        None,
                                        self.exports.close_is_requested(id),
                                        progress,
                                    ),
                                )
                                .2
                        })
                        .collect::<Vec<_>>();
                    let complete = finished.iter().all(|done| *done);
                    if return_when.satisfied(&finished) || tokio::time::Instant::now() >= deadline {
                        // Only read now, and only here: this is the one answer
                        // that has to be the child's own report.
                        let ids: Vec<String> =
                            summaries.iter().map(|(id, _, _)| id.clone()).collect();
                        let reports = tokio::task::spawn_blocking(move || {
                            ids.into_iter()
                                .map(|id| {
                                    crate::database::load_materialized_finished_turn_message(&id)
                                        .map(|message| (id, message))
                                })
                                .collect::<Result<std::collections::BTreeMap<_, _>>>()
                        })
                        .await??;
                        let agents = summaries
                            .into_iter()
                            .map(|(id, summary, progress)| {
                                let record = self.exports.session_record(&id);
                                let (state, output, finished) = inputs.status(
                                    &id,
                                    subagent_status(
                                        record.as_ref(),
                                        summary.as_ref(),
                                        starts.get(&id),
                                        reports.get(&id).and_then(Option::as_deref),
                                        self.exports.close_is_requested(&id),
                                        &progress,
                                    ),
                                );
                                let mut entry =
                                    wait_agent_entry(&id, &state, output, finished, &progress);
                                inputs.annotate(&id, &mut entry);
                                mark_parked(&mut entry, record.as_ref());
                                entry
                            })
                            .collect::<Vec<_>>();
                        let unfinished = agents
                            .iter()
                            .filter(|agent| agent["finished"] != serde_json::Value::Bool(true))
                            .filter_map(|agent| agent["child_session_id"].as_str())
                            .map(str::to_owned)
                            .collect::<Vec<_>>();
                        // What the caller has waited, not what this execution
                        // has: a request picked up late, or executed again
                        // after a restart, already spent part of its budget.
                        let waited_seconds =
                            (mj_core::subagent::subagent_wait_timeout(*timeout_seconds)
                                - remaining
                                + started.elapsed())
                            .as_secs();
                        tracing::info!(
                            parent_session_id,
                            complete,
                            waited_seconds,
                            "answering a sub-agent wait"
                        );
                        // A deadline reached is an answer, not a failure: the
                        // shape says which children are still running and what
                        // the caller should do next.
                        return Ok(serde_json::json!({
                            "status": if complete {
                                mj_core::subagent::WAIT_STATUS_COMPLETE
                            } else {
                                mj_core::subagent::WAIT_STATUS_STILL_RUNNING
                            },
                            "waited_seconds": waited_seconds,
                            "agents": agents,
                            "next_action": mj_core::subagent::next_action(
                                child_session_ids,
                                &unfinished,
                            ),
                        }));
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            SubagentToolAction::InterruptAgent { child_session_id } => {
                self.require_owned_child(parent_session_id, child_session_id)
                    .await?;
                let handle = self.session_handle(child_session_id.clone()).await?;
                let active = handle
                    .as_ref()
                    .and_then(|handle| handle.view().snapshot)
                    .and_then(|snapshot| snapshot.materialized.active_turn);
                let Some(active) = active else {
                    return Ok(
                        serde_json::json!({"child_session_id":child_session_id,"interrupted":false}),
                    );
                };
                let handle = handle.expect("active turn came from this handle");
                let result = handle
                    .submit(
                        format!("subagent-interrupt-{}", request.request_id),
                        RelayCommand::CancelTurnFor {
                            active_prompt_id: active.command_id.clone(),
                        },
                    )
                    .await;
                if let Err(error) = result {
                    // An explicit refusal because that turn already ended is a
                    // no-op, never permission to cancel the next turn instead.
                    if error
                        .downcast_ref::<mj_client::session::DeliveryUnconfirmed>()
                        .is_some()
                    {
                        return Err(error);
                    }
                    handle.sync_now().await?;
                    let still_active = handle
                        .view()
                        .snapshot
                        .and_then(|s| s.materialized.active_turn)
                        .is_some_and(|turn| turn.command_id == active.command_id);
                    if still_active {
                        return Err(error);
                    }
                    return Ok(
                        serde_json::json!({"child_session_id":child_session_id,"interrupted":false}),
                    );
                }
                Ok(serde_json::json!({"child_session_id":child_session_id,"interrupted":true}))
            }
            SubagentToolAction::CloseAgent { child_session_id } => {
                self.require_owned_child(parent_session_id, child_session_id)
                    .await?;
                Arc::clone(&self.exports)
                    .close_subagent(child_session_id.clone())
                    .await?;
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
                ensure!(
                    message.chars().count() <= mj_core::subagent::MAX_HANDBACK_CHARS,
                    "a report can be at most {} characters and this one has {}. Write the details \
                     to files in the report directory named in your first prompt, then call \
                     handback again with a short report that lists their paths",
                    mj_core::subagent::MAX_HANDBACK_CHARS,
                    message.chars().count()
                );
                let record = blocking("load sub-agent record", {
                    let child_id = child_id.clone();
                    move || crate::database::load_subagent(&child_id)
                })
                .await?;
                ensure!(
                    record.is_some_and(|record| record.handback_tool),
                    "handback is only for a Mjolnir sub-agent that was given the tool"
                );
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
                        move || crate::database::load_materialized_turn_outcome(&child_id)
                    })
                    .await?
                    .and_then(|(_, active, _)| active),
                };
                let turn = active_turn.context(
                    "no turn is running, so there is nothing to report on; hand back your report \
                     during the turn that did the work",
                )?;
                let recorded = blocking("record sub-agent report", {
                    let child_id = child_id.clone();
                    let handback = mj_core::subagent::SubagentHandback {
                        command_id: turn.command_id,
                        message: message.clone(),
                        recorded_at_ms: mj_core::clock::epoch_millis(),
                    };
                    move || crate::database::record_subagent_handback(&child_id, &handback)
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
    /// false. A failure leaves the child parked, and its message says what
    /// failed, including a container out of process slots.
    async fn unpark_child(&self, child_id: &str) -> Result<bool> {
        let parked = self
            .exports
            .session_record(child_id)
            .is_some_and(|record| record.state == SessionState::Parked)
            || self.exports.subagent_park_running(child_id);
        if !parked {
            return Ok(false);
        }
        Arc::clone(&self.exports)
            .unpark_subagent(child_id.to_owned())
            .await
            .context(
                "could not start the parked sub-agent again; it is still parked, so you can retry",
            )?;
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
            .context("the restarted sub-agent did not reattach")?;
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
            ensure!(
                tokio::time::Instant::now() < deadline,
                "the restarted sub-agent was not ready for a prompt within {} seconds",
                UNPARK_ATTACH_TIMEOUT.as_secs()
            );
            let _ = tokio::time::timeout(START_POLL, handle.changed()).await;
        }
    }

    async fn require_owned_child(&self, parent_id: &str, child_id: &str) -> Result<()> {
        anyhow::ensure!(
            self.list_subagents(parent_id.to_owned())
                .await?
                .iter()
                .any(|child| child.child_session_id == child_id),
            "session {child_id} does not belong to parent {parent_id}"
        );
        Ok(())
    }

    /// Record that a child finished a turn as a one-line user-visible notice
    /// in the parent's conversation. This is the one unsolicited sub-agent
    /// event, so it is a notice, not a prompt: it must not forge a user turn
    /// or start one. The child's output is not included — it is collected
    /// with `wait` and read in the child transcript; the notice only says
    /// what happened.
    ///
    /// `child_title` is the child's listed title. The turn is named by the
    /// number the child's own `mj wait` and `mj prompt` print (its accepted
    /// ordinal), and the outcome in words (R11-3).
    pub async fn record_subagent_completion_notice(
        &self,
        parent_session_id: String,
        child_session_id: &str,
        child_title: &str,
        outcome: &mj_core::state::MaterializedTurnOutcome,
    ) -> Result<()> {
        ensure!(
            !matches!(
                self.start_status(parent_session_id.clone()).await?,
                Some(StartStatus::Pending)
            ),
            "session initialization is still running"
        );
        let handle = self
            .sessions
            .session(parent_session_id.clone())
            .await
            .with_context(|| format!("session {parent_session_id} is not running"))?;
        let turn = match outcome.accepted_ordinal {
            Some(turn) => format!("turn {turn}"),
            None => "a turn".to_owned(),
        };
        submit_notice(
            &handle,
            format!(
                "Subagent \"{child_title}\" ({}) finished {turn} ({}).",
                mj_core::state::short_id(child_session_id),
                outcome.outcome
            ),
        )
        .await?;
        Ok(())
    }

    /// Remind a child that ended its turn without handing back a report,
    /// once, and say whether a reminder went out. The parent's completion
    /// notice waits for the reminder turn when one did.
    ///
    /// The caller passes the turn and what is queued or running from the live
    /// snapshot it just saw: the store's copy can lag it, and deciding from an
    /// older turn could skip a reminder a `wait` is counting on.
    pub async fn remind_subagent_to_hand_back(
        &self,
        child_session_id: &str,
        handback_tool: bool,
        last_turn: &mj_core::state::MaterializedTurnOutcome,
        in_flight: &[String],
    ) -> Result<bool> {
        // A prompt already queued or running is the child's next task; its
        // report supersedes this turn's.
        if !handback_tool || !in_flight.is_empty() {
            return Ok(false);
        }
        let report = blocking("load sub-agent report", {
            let child_id = child_session_id.to_owned();
            move || crate::database::load_subagent_report(&child_id)
        })
        .await?;
        let state = mj_core::subagent::report_state(
            handback_tool,
            &report,
            Some(last_turn),
            &[],
            mj_core::clock::epoch_millis(),
        );
        if state != (ReportState::Pending { remind: true }) {
            return Ok(false);
        }
        let command_id = new_command_id(mj_core::subagent::HANDBACK_REMINDER_PREFIX)?;
        let submitted = async {
            let handle = self
                .sessions
                .session(child_session_id.to_owned())
                .await
                .with_context(|| format!("session {child_session_id} is not running"))?;
            handle
                .submit(
                    command_id.clone(),
                    RelayCommand::Prompt {
                        prompt: vec![ContentBlock::Text(TextContent::new(
                            mj_core::subagent::HANDBACK_REMINDER_TEXT,
                        ))],
                    },
                )
                .await
        }
        .await;
        let child_id = child_session_id.to_owned();
        let for_command_id = last_turn.command_id.clone();
        match submitted {
            Ok(_) => {
                let reminder = mj_core::subagent::HandbackReminder {
                    command_id,
                    for_command_id,
                    sent_at_ms: mj_core::clock::epoch_millis(),
                };
                blocking("record handback reminder", move || {
                    crate::database::record_handback_reminder(&child_id, &reminder)
                })
                .await?;
                Ok(true)
            }
            Err(error) => {
                tracing::warn!(
                    child_session_id,
                    error = format!("{error:#}"),
                    "could not remind a sub-agent to hand back its report"
                );
                // Its last message stands as its report, so no wait keeps
                // waiting for a reminder that never went out.
                blocking("record failed handback reminder", move || {
                    crate::database::record_handback_reminder_failed(&child_id, &for_command_id)
                })
                .await?;
                Ok(false)
            }
        }
    }

    /// Park a child whose turn ended once its parent has been told: stop its
    /// worker so it holds no processes in the parent's container, keeping its
    /// record, conversation and report (#1161). A park never fails the parent
    /// or the notice: a failure is logged and the child stays live, counting
    /// toward its parent's cap.
    pub async fn park_subagent(&self, child_session_id: &str) {
        if let Err(error) = Arc::clone(&self.exports)
            .park_subagent(child_session_id.to_owned())
            .await
        {
            tracing::warn!(
                child_session_id,
                error = format!("{error:#}"),
                "could not park a sub-agent whose turn ended; it stays live"
            );
        }
    }

    /// Forget sessions the daemon no longer holds a record for, so a
    /// long-running daemon does not accumulate one entry per session ever
    /// created through the API.
    fn prune_starts(&self) {
        let mut starts = self.starts.lock().expect("api start status mutex poisoned");
        starts.retain(|session_id, start| {
            let present = (self.session_states)(session_id).is_some();
            if !present && let Some(task) = &start.task {
                start.cancel.cancel();
                tracing::debug!(
                    session_id,
                    task_finished = task.is_finished(),
                    "cancel forgotten API start"
                );
            }
            present
        });
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
                "session {session_id} has no live target; export its checkpoint bundle instead"
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

fn profile_remaining_percent(report: Option<&ProfileQuota>) -> Option<u8> {
    let report = report.filter(|report| report.error.is_none())?;
    if report.is_usage_priced() {
        return Some(100);
    }
    report
        .windows
        .iter()
        .filter_map(|window| window.remaining_percent)
        .min()
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
/// A close is not instant: it cancels whatever owned the session, checkpoints,
/// seals the relay and tears the child's process tree down, and the record only
/// says `Closing` once that is under way. Without this, a child whose turn had
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
    if closing && record.is_some_and(|record| record.state != SessionState::Stopped) {
        return ("stopping".into(), None, false);
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
                    && (progress.awaiting_prompt(match start {
                        Some(StartStatus::Submitted { turn_id }) => Some(*turn_id),
                        _ => None,
                    }) || matches!(progress.report, ReportState::Pending { .. })) =>
            {
                ("running".into(), summary.last_agent_message.clone(), false)
            }
            Some(summary) if idle(summary) => {
                match (
                    &progress.report,
                    &progress.login_failure,
                    &progress.failed_turn,
                ) {
                    (ReportState::Delivered(message), _, _) => {
                        ("completed".into(), Some(message.clone()), true)
                    }
                    // A child whose profile could not sign in failed for a
                    // reason its parent can fix, not for anything in its task
                    // (#1160), so the answer names the profile and the fix.
                    (_, Some(profile_id), _) => (
                        "failed".into(),
                        Some(format!(
                            "profile {profile_id}: {}",
                            mj_core::subagent::login_invalid_reason(profile_id)
                        )),
                        true,
                    ),
                    // A turn that failed says so, with its reason: it is not a
                    // report, and the child can be given another prompt.
                    (_, None, Some((state, reason))) => {
                        ((*state).to_owned(), Some(reason.clone()), true)
                    }
                    _ => (
                        "completed".into(),
                        finished_turn_message
                            .map(str::to_owned)
                            .or_else(|| summary.last_agent_message.clone()),
                        true,
                    ),
                }
            }
            Some(summary) => ("running".into(), summary.last_agent_message.clone(), false),
            None => ("preparing".into(), None, false),
        },
    }
}

/// What the store says about a child's answer to its parent's newest prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChildProgress {
    /// Where the child's report stands after its last finished turn.
    pub report: ReportState,
    /// The newest prompt the parent gave the child, by acceptance ordinal.
    pub awaited_ordinal: Option<u64>,
    /// Acceptance ordinal of the child's last finished turn.
    pub answered_ordinal: Option<u64>,
    /// How that turn failed, when it did: `failed` or `interrupted`, and why.
    pub failed_turn: Option<(&'static str, String)>,
    /// The child's profile, when that turn failed because the provider
    /// refused the profile's login.
    pub login_failure: Option<String>,
    /// The directory on the parent's target where the child writes the
    /// details its report points to.
    pub report_dir: Option<String>,
}

impl ChildProgress {
    /// A child whose last turn answered everything asked and ended normally,
    /// with this report.
    #[cfg(test)]
    pub(crate) fn settled(report: ReportState) -> Self {
        Self {
            report,
            awaited_ordinal: None,
            answered_ordinal: None,
            failed_turn: None,
            login_failure: None,
            report_dir: None,
        }
    }

    /// Whether the parent's newest prompt is still unanswered. `submitted` is
    /// the first prompt's ordinal while its start follow-up remembers it.
    fn awaiting_prompt(&self, submitted: Option<u64>) -> bool {
        self.awaited_ordinal.max(submitted).is_some_and(|awaited| {
            self.answered_ordinal
                .is_none_or(|answered| answered < awaited)
        })
    }
}

/// Read a child's progress from the store. The sub-agent `wait` and
/// `list_agents` apply this one rule; the session wait and the reminder apply
/// the same report rule to the turn they saw.
pub(crate) fn load_child_progress(child_id: &str) -> Result<ChildProgress> {
    let subagent = crate::database::load_subagent(child_id)?;
    let handback_tool = subagent.as_ref().is_some_and(|record| record.handback_tool);
    let recorded = crate::database::load_subagent_report(child_id)?;
    let (active, last) = crate::database::load_materialized_turn_outcome(child_id)?
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
        &in_flight,
        mj_core::clock::epoch_millis(),
    );
    let failed_turn = match last.as_ref() {
        // Only a failed turn pays for reading its last message.
        Some(turn) if mj_core::subagent::failed_turn(turn, None).is_some() => {
            let message = crate::database::load_materialized_finished_turn_message(child_id)?;
            mj_core::subagent::failed_turn(turn, message.as_deref())
        }
        _ => None,
    };
    let login_failure = last
        .as_ref()
        .filter(|turn| mj_core::subagent::turn_failed_on_login(turn))
        .and(subagent.map(|record| record.profile_id));
    Ok(ChildProgress {
        report,
        awaited_ordinal: recorded.awaited_ordinal,
        answered_ordinal: last.as_ref().and_then(|turn| turn.accepted_ordinal),
        failed_turn,
        login_failure,
        report_dir: recorded.report_dir,
    })
}

/// One child's entry in a `wait` answer. A handback is already within the
/// cap; a last message that stands in for one is not, so every output is
/// bounded here and marked when it was cut.
fn wait_agent_entry(
    id: &str,
    state: &str,
    output: Option<String>,
    finished: bool,
    progress: &ChildProgress,
) -> serde_json::Value {
    let output = output.map(|output| bounded_report(&output));
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
    if state == "failed"
        && let Some(profile_id) = &progress.login_failure
    {
        agent["failure"] = serde_json::json!({
            "kind": "login_invalid",
            "profile_id": profile_id,
        });
    }
    agent
}

/// Say in a `wait` or `list_agents` entry that the child is parked: its turn
/// ended and its worker is stopped, so it holds no processes, and the next
/// `send_input` starts it again. Its state is reported as for any finished
/// child; only a parked child carries the field.
fn mark_parked(entry: &mut serde_json::Value, record: Option<&mj_core::state::SessionRecord>) {
    if record.is_some_and(|record| record.state == SessionState::Parked) {
        entry["parked"] = serde_json::Value::Bool(true);
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

/// Whether a session is still on its way to being usable.
///
/// Any other state means the launch ended — stopped, closing, lost or failed —
/// so the follow-up stops rather than waiting out its deadline. A record that
/// is not published yet is treated as still starting; the deadline bounds it.
///
/// A session that failed to start already stored why, so the message carries
/// that cause. Without it the caller is told the symptom it can already see
/// and nothing about the reason, which is what #1065 reported.
fn still_starting(
    states: &SessionStateSource,
    exports: &Arc<dyn ExportRuntime>,
    session_id: &str,
) -> Result<()> {
    match states(session_id) {
        Some(
            SessionState::Provisioning
            | SessionState::Running
            | SessionState::Disconnected
            | SessionState::Checkpointing,
        )
        | None => Ok(()),
        Some(state) => match exports
            .session_record(session_id)
            .and_then(|record| record.last_error)
        {
            Some(cause) => {
                bail!("session {session_id} is {state:?} and will not take a first prompt: {cause}")
            }
            None => {
                bail!("session {session_id} is {state:?} and will not take a first prompt")
            }
        },
    }
}

/// Apply model, effort, and the first prompt to a session that is still coming
/// up, returning the turn the prompt was accepted as.
async fn apply_followup(
    sessions: SessionControl,
    states: SessionStateSource,
    exports: Arc<dyn ExportRuntime>,
    session_id: String,
    followup: StartFollowup,
) -> Result<Option<u64>> {
    let deadline = tokio::time::Instant::now() + START_DEADLINE;
    let mut handle = loop {
        still_starting(&states, &exports, &session_id)?;
        ensure!(
            tokio::time::Instant::now() < deadline,
            "session {session_id} had no live actor within 30 minutes"
        );
        if let Ok(handle) = sessions.wait_for_session(&session_id, START_POLL).await {
            break handle;
        }
    };

    // Setting a configuration option needs the harness's own session, not just
    // a connected worker: the options it accepts arrive with it.
    let needs_config = followup.model.is_some() || followup.effort.is_some() || followup.fast_mode;
    loop {
        let view = handle.view();
        if let Some(ViewError::TargetMissing(detail)) = &view.error {
            bail!("session {session_id} lost its target: {detail}");
        }
        match &view.snapshot {
            Some(snapshot)
                if view.connected
                    && (!needs_config || snapshot.operational.native_session_is_ready()) =>
            {
                break;
            }
            _ => {}
        }
        still_starting(&states, &exports, &session_id)?;
        ensure!(
            tokio::time::Instant::now() < deadline,
            "session {session_id} was not ready for its first prompt within 30 minutes"
        );
        // Bounded so a session that dies quietly is still noticed by the
        // record check above rather than waiting for a change that never comes.
        let _ = tokio::time::timeout(START_POLL, handle.changed()).await;
    }

    for (key, value) in [("model", followup.model), ("effort", followup.effort)] {
        let Some(value) = value else {
            continue;
        };
        let snapshot = handle
            .view()
            .snapshot
            .context("session configuration is unavailable")?;
        let choices =
            mj_core::acp::session_config_choices(&snapshot.operational.config_options, key);
        ensure!(
            choices.iter().any(|choice| choice.value == value),
            "this agent does not offer {value} as a {key}"
        );
        handle.set_config(key.to_owned(), value).await?;
    }

    // Fast mode is best effort: a Luna sub-agent should start fast, but a
    // spawn must not fail just because the agent does not offer the option
    // or the set_config call itself fails.
    if followup.fast_mode {
        let offers_on = handle.view().snapshot.is_some_and(|snapshot| {
            mj_core::acp::session_config_choices(&snapshot.operational.config_options, "fast-mode")
                .iter()
                .any(|choice| choice.value == "on")
        });
        if offers_on
            && let Err(error) = handle
                .set_config("fast-mode".to_owned(), "on".to_owned())
                .await
        {
            tracing::warn!(
                %session_id,
                %error,
                "could not turn on Codex fast mode for a Luna sub-agent"
            );
        }
    }

    still_starting(&states, &exports, &session_id)?;
    match followup.prompt {
        Some(text) => Ok(Some(submit_prompt(&handle, text).await?)),
        None => Ok(None),
    }
}

/// Submit one prompt as a single text block, returning its acceptance ordinal.
async fn submit_prompt(handle: &SessionHandle, text: String) -> Result<u64> {
    handle
        .submit(
            new_command_id("api")?,
            RelayCommand::Prompt {
                prompt: vec![ContentBlock::Text(TextContent::new(text))],
            },
        )
        .await
}

/// Record one Mjolnir notice line in a session's conversation. Unlike a
/// prompt, a notice never starts a turn, so it is how the daemon tells a
/// parent that a sub-agent finished without acting as that parent's user.
async fn submit_notice(handle: &SessionHandle, text: String) -> Result<u64> {
    handle
        .submit(
            new_command_id("subagent")?,
            RelayCommand::RecordNotice { text },
        )
        .await
}

/// Run one blocking database read off the async runtime, keeping its context.
async fn blocking<T, F>(label: &'static str, job: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(job)
        .await
        .with_context(|| format!("{label} task panicked"))?
}

fn checkpoint_export_error(error: anyhow::Error) -> ExportError {
    if crate::controller::checkpoint_was_deferred(&error) {
        ExportError::Refused(format!("{error:#}"))
    } else {
        ExportError::Failed(error)
    }
}

/// Run one blocking export step off the async runtime.
async fn export_blocking<T, F>(label: &'static str, job: F) -> Result<T, ExportError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(job).await {
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

/// Checkpoint a session in a task that owns the work to the end.
///
/// A checkpoint marks the session `Checkpointing` in the database before it
/// starts and holds a barrier on the worker while it runs. Awaiting it inline
/// in an HTTP handler meant a client that gave up part-way dropped the handler
/// future and abandoned the capture there, leaving the session marked busy
/// with nothing left to finish or fail it, so every retry was refused for
/// minutes (#1010). The spawned task keeps running whether or not anyone is
/// still waiting for its answer, and the guard it holds is released when it
/// ends. The outer task reports a failure no requester is left to receive.
async fn supervised_checkpoint(
    exports: Arc<dyn ExportRuntime>,
    session_id: String,
) -> Result<mj_core::state::CheckpointMetadata> {
    let upgrade_work = crate::upgrade::activity("API checkpoint")?;
    let checkpoint = tokio::spawn(async move {
        let _upgrade_work = upgrade_work;
        exports.checkpoint_now(session_id).await
    });
    tokio::spawn(async move {
        let result = match checkpoint.await {
            Ok(result) => result,
            Err(error) => Err(anyhow!("the session checkpoint task failed: {error}")),
        };
        if let Err(error) = &result {
            tracing::warn!(?error, "API bundle export checkpoint failed");
        }
        result
    })
    .await
    .unwrap_or_else(|error| Err(anyhow!("the session checkpoint task failed: {error}")))
}

/// The directory on the target the session's agent runs in.
///
/// It is the primary repository's directory for every target kind: a bundle
/// session's harness is launched in `<workspace root>/<primary destination>`,
/// and a bare project session's in its selected project directory, whose
/// parent is the layout's workspace root. A relative export path resolves
/// here so it means what it meant to the agent that wrote the file (#1079).
fn agent_working_directory(layout: &SessionExportLayout) -> Result<String, ExportError> {
    Ok(target_join(
        &layout.workspace_root,
        &primary_repository(layout)?.relative_destination,
    ))
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
    if resolved.is_empty() {
        return Err(ExportError::Refused(format!(
            "{} names {root} itself, not a file in it",
            relative.display()
        )));
    }
    Ok((root, resolved.join("/")))
}

/// Join a relative path onto a target-side root.
///
/// Target paths are POSIX text whatever this daemon runs on, so the components
/// are spelled with `/` here rather than by the host's separator.
fn target_join(root: &str, relative: &Path) -> String {
    let mut path = root.trim_end_matches('/').to_owned();
    for component in relative.components() {
        if let Component::Normal(part) = component {
            path.push('/');
            path.push_str(&part.to_string_lossy());
        }
    }
    path
}

async fn write_workspace_file(
    exports: Arc<dyn ExportRuntime>,
    session_id: String,
    path: PathBuf,
    bytes: Vec<u8>,
    overwrite: bool,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), ExportError> {
    use std::sync::atomic::Ordering;
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
    let mut lease = crate::controller::IdleWorkspaceLease::acquire(&handle, record.harness_kind)
        .await
        .map_err(|e| ExportError::Refused(format!("{e:#}")))?;
    if cancelled.load(Ordering::Acquire) {
        return Err(ExportError::Refused("file upload cancelled".into()));
    }
    let worker_cancelled = cancelled.clone();
    let mut transfer = tokio::task::spawn_blocking(move || {
        let binary = format!(
            "{}/hel",
            targets::worker_root(&layout.backend, &session_id)?
        );
        let mut argv = vec![
            binary,
            "worker".into(),
            "write-file".into(),
            "--length".into(),
            bytes.len().to_string(),
            "--root".into(),
            root,
            "--path".into(),
            relative,
        ];
        if overwrite {
            argv.push("--overwrite".into());
        }
        let command =
            targets::command_on_locator(&layout.backend, &session_id, argv, "session file write")?
                .with_sensitive_stdin(bytes);
        CancellableProcessExecutor::new(worker_cancelled)
            .with_deadline(EXPORT_TIMEOUT)
            .execute(&command)
    });
    let output = loop {
        tokio::select! {
            output = &mut transfer => break output.map_err(|e| ExportError::Failed(e.into()))?.map_err(ExportError::Failed)?,
            () = tokio::time::sleep(Duration::from_millis(250)) => {
                if let Err(error) = tokio::time::timeout(Duration::from_secs(10), lease.verify()).await.context("checking file write barrier timed out").and_then(|r| r) {
                    cancelled.store(true, Ordering::Release);
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
    let result = worker_output(output, "session file write").map(|_| ());
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
    let output: CommandOutput = export_blocking(purpose, move || {
        let binary = format!(
            "{}/hel",
            targets::worker_root(&layout.backend, &session_id)?
        );
        let mut argv = vec![binary, "worker".to_owned()];
        argv.extend(arguments);
        let command = targets::command_on_locator(&layout.backend, &session_id, argv, purpose)?;
        CancellableProcessExecutor::with_timeout(EXPORT_TIMEOUT).execute(&command)
    })
    .await?;
    worker_output(output, purpose)
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

impl SubagentBackend for ApiBackend {
    fn subagent_report(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<Option<(bool, mj_core::subagent::SubagentReport)>>> {
        Box::pin(async move {
            blocking("load sub-agent report", move || {
                let Some(record) = crate::database::load_subagent(&session_id)? else {
                    return Ok(None);
                };
                let report = crate::database::load_subagent_report(&session_id)?;
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
            let workspace = tokio::task::spawn_blocking(move || {
                crate::database::create_or_get_workspace(&name)
            })
            .await??;
            self.exports.refresh_workspaces().await?;
            Ok(workspace)
        })
    }

    fn subagent_candidates(
        &self,
        parent_profile: String,
    ) -> BoxFuture<'_, Result<crate::server::api::SubagentCandidates>> {
        Box::pin(async move {
            let mut candidates = crate::server::api::SubagentCandidates::default();
            let mut ids = self.profile_catalog.candidates(&parent_profile)?;
            // A profile whose login the provider has refused would start a
            // child that dies on its first request (#1160), so it is refused
            // here, with the fix, until its login file changes.
            {
                let rejected = self
                    .rejected_logins
                    .lock()
                    .map_err(|_| anyhow!("refused logins lock poisoned"))?;
                ids.retain(|(profile_id, _)| match rejected.refusal(profile_id) {
                    Some(reason) => {
                        candidates.unavailable.push((profile_id.clone(), reason));
                        false
                    }
                    None => true,
                });
            }
            // One discovery per profile, so a profile whose harness cannot be
            // discovered drops out on its own instead of failing the answer.
            let discoveries = futures::future::join_all(
                ids.iter()
                    .map(|(id, _)| self.profile_catalog.capabilities(std::slice::from_ref(id))),
            )
            .await;
            let quota_reports = self
                .quota_reports
                .lock()
                .map_err(|_| anyhow!("sub-agent quota reports lock poisoned"))?;
            for ((profile_id, harness), discovery) in ids.into_iter().zip(discoveries) {
                match discovery.map(|mut choices| choices.pop()) {
                    Ok(Some(choices)) => {
                        let remaining_percent =
                            profile_remaining_percent(quota_reports.get(&profile_id));
                        candidates
                            .offered
                            .push(crate::server::api::SubagentCandidate {
                                profile_id,
                                harness,
                                choices,
                                remaining_percent,
                            });
                    }
                    Ok(None) => candidates
                        .unavailable
                        .push((profile_id, "its discovery returned nothing".to_owned())),
                    Err(error) => {
                        tracing::warn!(profile_id, %error, "sub-agent profile left out");
                        candidates
                            .unavailable
                            .push((profile_id, format!("{error:#}")));
                    }
                }
            }
            if candidates.offered.is_empty() && !candidates.unavailable.is_empty() {
                bail!(
                    "no sub-agent profile is available: {}",
                    candidates
                        .unavailable
                        .iter()
                        .map(|(id, reason)| format!("{id} ({reason})"))
                        .collect::<Vec<_>>()
                        .join("; ")
                );
            }
            Ok(candidates)
        })
    }

    fn wiki_sync_is_stale(&self) -> bool {
        self.exports.wiki_sync_is_stale()
    }

    fn wiki_request_sync(&self) {
        self.exports.wiki_request_sync();
    }

    fn wiki_search(&self, query: String, limit: usize) -> BoxFuture<'_, Result<WikiSearchPage>> {
        let runtime = Arc::clone(&self.exports);
        Box::pin(async move { runtime.wiki_search(query, limit).await })
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

    fn wiki_restore(&self, request: WikiRestoreRequest) -> BoxFuture<'_, Result<Option<String>>> {
        let runtime = Arc::clone(&self.exports);
        Box::pin(async move { runtime.wiki_restore(request).await })
    }

    fn start_subagent(
        &self,
        request: crate::controller::RegisterSubagentRequest,
    ) -> BoxFuture<'_, Result<mj_core::subagent::SubagentRecord>> {
        let runtime = Arc::clone(&self.exports);
        Box::pin(async move { runtime.spawn_subagent(request).await })
    }

    fn cancel_start(&self, session_id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if let Some(start) = self
                .starts
                .lock()
                .expect("api start status mutex poisoned")
                .remove(&session_id)
            {
                start.cancel.cancel();
            }
            Ok(())
        })
    }

    fn set_config(
        &self,
        session_id: String,
        key: String,
        value: String,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            ensure!(
                !matches!(
                    self.start_status(session_id.clone()).await?,
                    Some(StartStatus::Pending)
                ),
                "session initialization is still running"
            );
            let handle = self.sessions.session(session_id.clone()).await?;
            handle.set_config(key, value).await?;
            self.starts
                .lock()
                .expect("api start status mutex poisoned")
                .remove(&session_id);
            Ok(())
        })
    }

    fn session_handle(&self, session_id: String) -> BoxFuture<'_, Result<Option<SessionHandle>>> {
        Box::pin(async move { Ok(self.sessions.session(session_id).await.ok()) })
    }

    fn prompt(&self, session_id: String, text: String) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            ensure!(
                !matches!(
                    self.start_status(session_id.clone()).await?,
                    Some(StartStatus::Pending)
                ),
                "session initialization is still running"
            );
            let handle = self
                .sessions
                .session(session_id.clone())
                .await
                .with_context(|| format!("session {session_id} is not running"))?;
            let turn = submit_prompt(&handle, text).await?;
            self.starts
                .lock()
                .expect("api start status mutex poisoned")
                .remove(&session_id);
            Ok(turn)
        })
    }

    fn runtime_receipt(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<Option<mj_core::harness_runtime::RuntimeReceipt>>> {
        Box::pin(async move {
            blocking("load runtime receipt", move || {
                crate::database::load_runtime_receipt(&session_id)
            })
            .await
        })
    }

    fn turn_state(&self, session_id: String) -> BoxFuture<'_, Result<Option<TurnState>>> {
        Box::pin(async move {
            blocking("load turn outcome", move || {
                Ok(
                    crate::database::load_materialized_turn_outcome(&session_id)?.map(
                        |(execution, active_turn, last_turn_outcome)| TurnState {
                            execution,
                            active_turn,
                            last_turn_outcome,
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
                crate::database::load_materialized_turn_summary(
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
            if followup == StartFollowup::default() {
                return Ok(());
            }
            self.prune_starts();
            let sessions = self.sessions.clone();
            let states = self.session_states.clone();
            let exports = Arc::clone(&self.exports);
            let starts = Arc::clone(&self.starts);
            let starts_changed = Arc::clone(&self.starts_changed);
            let id = session_id.clone();
            let cancel = tokio_util::sync::CancellationToken::new();
            // Recorded before the work starts: a follow-up that finishes
            // immediately must find its entry to write its outcome into.
            self.starts
                .lock()
                .expect("api start status mutex poisoned")
                .insert(
                    session_id.clone(),
                    Start {
                        status: StartStatus::Pending,
                        task: None,
                        cancel: cancel.clone(),
                    },
                );
            let followup_id = session_id.clone();
            let upgrade_work = crate::upgrade::activity("API startup followup")?;
            let work = tokio::spawn(async move {
                tokio::select! {
                    result = apply_followup(sessions, states, exports, followup_id, followup) => result,
                    () = cancel.cancelled() => anyhow::bail!("session startup cancelled"),
                }
            });
            // A second task supervises the first so a panic in the follow-up
            // becomes a failure the caller's wait reports, rather than an
            // entry that stays Pending for as long as the daemon runs.
            let task = tokio::spawn(async move {
                let _upgrade_work = upgrade_work;
                let status = match work.await {
                    Ok(Ok(Some(turn_id))) => {
                        // A sub-agent child's wait must not read it as done
                        // before a finished turn reaches its first prompt. A
                        // session that is not a child records nothing.
                        let child_id = id.clone();
                        match tokio::task::spawn_blocking(move || {
                            crate::database::record_subagent_prompt(&child_id, turn_id)
                        })
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(error)) => {
                                tracing::warn!(%error, "record a sub-agent's first prompt")
                            }
                            Err(error) => {
                                tracing::error!(%error, "sub-agent prompt recorder task failed")
                            }
                        }
                        Some(StartStatus::Submitted { turn_id })
                    }
                    // Configuration applied and nothing to submit: there is no
                    // turn to report, so the session is an ordinary one again.
                    Ok(Ok(None)) => None,
                    Ok(Err(error)) => Some(StartStatus::Failed {
                        message: format!("{error:#}"),
                    }),
                    Err(error) => Some(StartStatus::Failed {
                        message: format!("starting session {id} failed: {error}"),
                    }),
                };
                if let Some(StartStatus::Failed { message }) = &status {
                    let failed_id = id.clone();
                    let message = message.clone();
                    match tokio::task::spawn_blocking(move || {
                        crate::database::record_api_error(failed_id, message)
                    })
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::warn!(%error, "persist startup API error"),
                        Err(error) => {
                            tracing::error!(%error, "startup API error recorder task failed")
                        }
                    }
                }
                let mut starts = starts.lock().expect("api start status mutex poisoned");
                match status {
                    Some(status) => {
                        if let Some(start) = starts.get_mut(&id) {
                            start.status = status;
                        }
                    }
                    None => {
                        starts.remove(&id);
                    }
                }
                starts_changed.notify_waiters();
            });
            if let Some(start) = self
                .starts
                .lock()
                .expect("api start status mutex poisoned")
                .get_mut(&session_id)
            {
                start.task = Some(task);
            }
            Ok(())
        })
    }

    fn start_status(&self, session_id: String) -> BoxFuture<'_, Result<Option<StartStatus>>> {
        Box::pin(async move {
            Ok(self
                .starts
                .lock()
                .expect("api start status mutex poisoned")
                .get(&session_id)
                .map(|start| start.status.clone()))
        })
    }

    fn transcript(
        &self,
        session_id: String,
        after_seq: u64,
        limit: usize,
        role: Option<mj_core::transcript::TranscriptRole>,
    ) -> BoxFuture<'_, Result<Option<TranscriptPage>>> {
        Box::pin(async move {
            blocking("load transcript page", move || {
                crate::database::load_materialized_transcript_filtered(
                    &session_id,
                    after_seq,
                    limit,
                    role,
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
            let repository = agent_working_directory(&layout)?;
            let mut arguments = vec!["diff".to_owned(), "--repository".to_owned(), repository];
            if let Some(base) = options.base {
                arguments.push(format!("--base={base}"));
            } else if let Some(worktree) = &layout.managed_worktree {
                match &worktree.base_commit {
                    Some(base) => {
                        arguments.push("--base".to_owned());
                        arguments.push(base.clone());
                    }
                    // Sessions created before the base was recorded still name
                    // their branch, whose reflog says where it started.
                    None => {
                        arguments.push("--branch".to_owned());
                        arguments.push(worktree.branch.clone());
                    }
                }
            }
            if options.json {
                arguments.push("--json".to_owned());
            }
            let stdout = worker_command(layout, session_id, arguments, "session diff").await?;
            String::from_utf8(stdout).map_err(|error| {
                ExportError::Failed(anyhow!("the session diff was not UTF-8: {error}"))
            })
        })
    }

    fn read_file(
        &self,
        session_id: String,
        path: PathBuf,
    ) -> BoxFuture<'_, std::result::Result<Vec<u8>, ExportError>> {
        Box::pin(async move {
            self.require_live_target(&session_id)?;
            let layout = export_layout(session_id.clone()).await?;
            // The path resolves in the agent's directory; how far `..` may
            // reach depends on whether the layout has a sibling repository to
            // reach (#1079).
            let (root, relative) = export_root_and_path(&layout, &path)?;
            let arguments = vec![
                "read-file".to_owned(),
                "--root".to_owned(),
                root,
                "--path".to_owned(),
                relative,
            ];
            worker_command(layout, session_id, arguments, "session file read").await
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
        bytes: Vec<u8>,
        overwrite: bool,
    ) -> BoxFuture<'_, Result<(), ExportError>> {
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
            let task = tokio::spawn(write_workspace_file(
                exports, session_id, path, bytes, overwrite, cancelled,
            ));
            tokio::spawn(async move {
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
            let layout = export_layout(session_id.clone()).await?;
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
            // A live session's work is only in the archive once it has been
            // checkpointed, so take a fresh checkpoint; a stopped session's
            // last checkpoint already holds everything it did.
            let archive_path = match record.target {
                Some(_) => match supervised_checkpoint(self.exports.clone(), session_id.clone())
                    .await
                {
                    Ok(checkpoint) => checkpoint.archive_path,
                    // The session has its own lifecycle operation in flight
                    // (resume/close/move); a fresh checkpoint would fight it, so
                    // export the last durable checkpoint, which predates that
                    // operation, rather than failing (#1010).
                    Err(error)
                        if error
                            .downcast_ref::<crate::daemon::SessionLifecycleBusy>()
                            .is_some() =>
                    {
                        record
                            .checkpoint
                            .ok_or_else(|| {
                                ExportError::Refused(format!(
                                    "{error}, and has no earlier checkpoint to export a bundle from"
                                ))
                            })?
                            .archive_path
                    }
                    Err(error) => return Err(checkpoint_export_error(error)),
                },
                None => {
                    record
                        .checkpoint
                        .ok_or_else(|| {
                            ExportError::Refused(
                                "this session has no checkpoint to export a bundle from".into(),
                            )
                        })?
                        .archive_path
                }
            };
            let bundles = export_blocking("verify the checkpoint bundles", move || {
                mj_checkpoint::archive::verify_repository_bundles_streaming(&archive_path)
            })
            .await?;
            let repository = bundles
                .repositories
                .iter()
                .find(|repository| repository.metadata.id == bundles.primary_repository)
                .ok_or_else(|| {
                    ExportError::Failed(anyhow!(
                        "the checkpoint has no repository {:?}",
                        bundles.primary_repository
                    ))
                })?;
            if repository.committed_bundle.is_empty() {
                return Err(ExportError::Refused(
                    "no commits beyond the session base".into(),
                ));
            }
            Ok(BundleExport {
                repository: repository.metadata.id.clone(),
                bytes: repository.committed_bundle.clone(),
            })
        })
    }
}

#[cfg(test)]
mod tests;

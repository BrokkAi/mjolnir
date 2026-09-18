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

use mj_core::config::HarnessKind;
use mj_core::state::{MaterializedExecutionState, SessionState};
use mj_core::subagent::{DEFAULT_WAIT_SECONDS, MAX_WAIT_SECONDS};

use crate::quota::ProfileQuota;

use crate::controller::{Controller, SessionExportLayout};
use crate::server::api::{
    BundleExport, ExportError, PushedBranch, StartFollowup, StartStatus, SubagentBackend,
    TranscriptPage, TurnSpan, TurnState, TurnSummary,
};
use crate::targets::{self, CancellableProcessExecutor, CommandExecutor, CommandOutput};
use mj_client::session::{BoxFuture, SessionControl, SessionHandle, ViewError, new_command_id};
use mj_core::relay::RelayCommand;

use crate::daemon::RuntimeState;
use mj_client::daemon::{WikiHitTranscript, WikiRestoreRequest, WikiSearchPage};

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

    /// Hand a changed workspace list to everything reading the daemon's, the
    /// same republication the daemon's own workspace actions perform. A
    /// backend built without a daemon has no one to publish to.
    fn republish_workspaces(&self, _workspaces: Vec<mj_core::workspace::WorkspaceRecord>) {}

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
        Box::pin(async move { self.close_session(session_id).await })
    }

    fn republish_workspaces(&self, workspaces: Vec<mj_core::workspace::WorkspaceRecord>) {
        RuntimeState::publish_workspaces(self, workspaces);
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
    /// Latest background-refreshed quota reports, used to choose one profile
    /// per harness without making the parent reason about credential aliases.
    quota_reports: Arc<Mutex<BTreeMap<String, ProfileQuota>>>,
    /// The capabilities `list_profiles` answers with. The catalogue discovers
    /// them in the background, so the call only filters and ranks what it
    /// holds, waiting for a profile the pass has not published yet.
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
            quota_reports: Arc::new(Mutex::new(BTreeMap::new())),
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
        match &request.action {
            SubagentToolAction::ListProfiles => {
                let parent = self
                    .exports
                    .session_record(parent_session_id)
                    .context("parent session disappeared")?;
                // The catalogue discovers profile capabilities in the
                // background, so this call only filters and ranks: it takes
                // the candidates the catalogue's configuration offers, ranks
                // them with the quota reports, and waits on the background
                // pass for the capabilities of the profiles it will quote.
                let candidates = self.profile_catalog.candidates(&parent.last_profile)?;
                let ids = {
                    let quota_reports = self
                        .quota_reports
                        .lock()
                        .map_err(|_| anyhow!("sub-agent quota reports lock poisoned"))?;
                    select_profile_per_harness(candidates, &quota_reports)
                };
                let wanted = ids.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
                let choices = self.profile_catalog.capabilities(&wanted).await?;
                let mut profiles = Vec::with_capacity(ids.len());
                for ((id, harness), choices) in ids.into_iter().zip(choices) {
                    profiles.push(serde_json::json!({
                        "profile_id":id,
                        "harness":harness.id(),
                        "default_model":choices.model,
                        "models":choices.models,
                        "efforts":choices.efforts,
                    }));
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
                let profile_id = profile_id.clone().unwrap_or(parent.last_profile.clone());
                let mut selected_model = model.clone();
                let mut selected_effort = effort.clone();
                if profile_id == parent.last_profile
                    && (selected_model.is_none() || selected_effort.is_none())
                    && let Some(handle) = self.session_handle(parent_session_id.to_owned()).await?
                    && let Some(snapshot) = handle.view().snapshot
                {
                    selected_model = selected_model
                        .or_else(|| snapshot.operational.config.get("model").cloned());
                    selected_effort = selected_effort
                        .or_else(|| snapshot.operational.config.get("effort").cloned());
                }
                // Checked against the warm catalogue only. Discovering a
                // profile launches a harness, which takes tens of seconds, and
                // the caller is a model waiting on its tool call. A selector
                // the catalogue could not check is validated by the start
                // follow-up against the child's live harness; an unsupported
                // one fails the child's start and is reported to the parent as
                // that child's error through `wait` and `list_agents`.
                if (selected_model.is_some() || selected_effort.is_some())
                    && let Some(choices) = self.profile_catalog.published(&profile_id)
                {
                    crate::server::api::validate_selectors(
                        &choices,
                        selected_model.as_deref(),
                        selected_effort.as_deref(),
                    )
                    .map_err(|failure| anyhow::anyhow!(failure.message))?;
                }
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
                let backend: Arc<dyn crate::server::api::SubagentBackend> = self.clone();
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
                        profile_id,
                        model: selected_model.clone(),
                        effort: selected_effort.clone(),
                        working_directory: working_directory.clone(),
                        initial_prompt: prompt.clone(),
                        request_key: request.request_id.clone(),
                    })
                    .await?;
                self.start_followup(
                    relation.child_session_id.clone(),
                    crate::server::api::StartFollowup {
                        model: selected_model,
                        effort: selected_effort,
                        prompt: Some(prompt),
                    },
                )
                .await?;
                Ok(serde_json::json!({
                    "child_session_id":relation.child_session_id,
                    "task_name":relation.task_name,
                    "profile_id":relation.profile_id,
                }))
            }
            SubagentToolAction::ListAgents => {
                let relations = self.list_subagents(parent_session_id.to_owned()).await?;
                let child_ids = relations
                    .iter()
                    .map(|relation| relation.child_session_id.clone())
                    .collect::<Vec<_>>();
                let summaries = tokio::task::spawn_blocking(move || {
                    child_ids
                        .into_iter()
                        .map(|id| {
                            crate::database::load_materialized_session_summary(&id)
                                .map(|summary| (id, summary))
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
                        let (state, _, _) = subagent_status(
                            record.as_ref(),
                            summaries
                                .get(&relation.child_session_id)
                                .and_then(Option::as_ref),
                            starts.get(&relation.child_session_id),
                            None,
                        );
                        serde_json::json!({
                            "child_session_id":relation.child_session_id,
                            "task_name":relation.task_name,
                            "profile_id":relation.profile_id,
                            "state":state,
                        })
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
                let parent_id = parent_session_id.to_owned();
                let child_id = child_session_id.clone();
                tokio::task::spawn_blocking(move || {
                    Controller::load()?.ensure_subagent_slot_available(&parent_id, &child_id)
                })
                .await??;
                let turn_id = self
                    .prompt(child_session_id.clone(), message.clone())
                    .await?;
                Ok(serde_json::json!({"child_session_id":child_session_id,"turn_id":turn_id}))
            }
            SubagentToolAction::WaitAgents {
                child_session_ids,
                timeout_seconds,
            } => {
                for child_id in child_session_ids {
                    self.require_owned_child(parent_session_id, child_id)
                        .await?;
                }
                let deadline = tokio::time::Instant::now()
                    + Duration::from_secs(
                        timeout_seconds
                            .unwrap_or(DEFAULT_WAIT_SECONDS)
                            .clamp(1, MAX_WAIT_SECONDS),
                    );
                loop {
                    let ids = child_session_ids.clone();
                    let summaries = tokio::task::spawn_blocking(move || {
                        ids.into_iter()
                            .map(|id| {
                                crate::database::load_materialized_session_summary(&id)
                                    .map(|summary| (id, summary))
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                    .await??;
                    let mut starts = std::collections::BTreeMap::new();
                    for (id, _) in &summaries {
                        if let Some(status) = self.start_status(id.clone()).await? {
                            starts.insert(id.clone(), status);
                        }
                    }
                    let complete = summaries.iter().all(|(id, summary)| {
                        let record = self.exports.session_record(id);
                        subagent_status(record.as_ref(), summary.as_ref(), starts.get(id), None).2
                    });
                    if complete || tokio::time::Instant::now() >= deadline {
                        // Only read now, and only here: this is the one answer
                        // that has to be the child's own report.
                        let ids: Vec<String> = summaries.iter().map(|(id, _)| id.clone()).collect();
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
                            .map(|(id, summary)| {
                                let record = self.exports.session_record(&id);
                                let (state, output, _) = subagent_status(
                                    record.as_ref(),
                                    summary.as_ref(),
                                    starts.get(&id),
                                    reports.get(&id).and_then(Option::as_deref),
                                );
                                serde_json::json!({"child_session_id":id,"state":state,"output":output})
                            })
                            .collect::<Vec<_>>();
                        return Ok(serde_json::json!({"agents":agents,"timed_out":!complete}));
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
            SubagentToolAction::InterruptAgent { child_session_id } => {
                self.require_owned_child(parent_session_id, child_session_id)
                    .await?;
                let handle = self
                    .session_handle(child_session_id.clone())
                    .await?
                    .context("child session has no live actor")?;
                handle
                    .submit(
                        crate::session_manager::new_command_id("subagent-interrupt")?,
                        mj_core::relay::RelayCommand::CancelTurn,
                    )
                    .await?;
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
    pub async fn record_subagent_completion_notice(
        &self,
        parent_session_id: String,
        child_session_id: &str,
        task_name: &str,
        turn: u64,
        outcome: &str,
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
        submit_notice(
            &handle,
            format!(
                "Subagent {task_name:?} ({}) finished turn {turn} ({outcome}).",
                mj_core::state::short_id(child_session_id)
            ),
        )
        .await?;
        Ok(())
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

fn select_profile_per_harness(
    mut candidates: Vec<(String, HarnessKind)>,
    quota_reports: &BTreeMap<String, ProfileQuota>,
) -> Vec<(String, HarnessKind)> {
    candidates.sort_by(|(left_id, left_harness), (right_id, right_harness)| {
        left_harness.cmp(right_harness).then_with(|| {
            profile_remaining_percent(quota_reports.get(right_id))
                .cmp(&profile_remaining_percent(quota_reports.get(left_id)))
                .then_with(|| left_id.cmp(right_id))
        })
    });
    candidates.dedup_by(|left, right| left.1 == right.1);
    candidates
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
fn subagent_status(
    record: Option<&mj_core::state::SessionRecord>,
    summary: Option<&mj_core::state::MaterializedSessionSummary>,
    start: Option<&StartStatus>,
    finished_turn_message: Option<&str>,
) -> (String, Option<String>, bool) {
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
            Some(summary) if matches!(summary.execution, MaterializedExecutionState::Idle) => (
                "completed".into(),
                finished_turn_message
                    .map(str::to_owned)
                    .or_else(|| summary.last_agent_message.clone()),
                true,
            ),
            Some(summary) => ("running".into(), summary.last_agent_message.clone(), false),
            None => ("preparing".into(), None, false),
        },
    }
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
            Some(cause) => bail!(
                "session {session_id} is {state:?} and will not take a first prompt: {cause}"
            ),
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
    let needs_config = followup.model.is_some() || followup.effort.is_some();
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

/// The directory on the target holding the session's primary repository.
fn primary_repository_path(layout: &SessionExportLayout) -> Result<String, ExportError> {
    let repository = layout
        .repositories
        .iter()
        .find(|repository| repository.id == layout.primary_repository)
        .ok_or_else(|| {
            ExportError::Failed(anyhow!(
                "session workspace has no repository {:?}",
                layout.primary_repository
            ))
        })?;
    Ok(target_join(
        &layout.workspace_root,
        &repository.relative_destination,
    ))
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
            layout.workspace_root,
            "--path".into(),
            target_join("", &path).trim_start_matches('/').into(),
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
            refusal_reason(&output.stderr, purpose),
        )),
        status => Err(ExportError::Failed(anyhow!(
            "{purpose} failed with status {status}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
}

/// The worker's own words for why it refused, or a plain statement when it
/// said nothing.
fn refusal_reason(stderr: &[u8], purpose: &str) -> String {
    let reason = String::from_utf8_lossy(stderr);
    let reason = reason.trim();
    match reason.is_empty() {
        true => format!("{purpose} was refused by the target"),
        false => reason.to_owned(),
    }
}

impl SubagentBackend for ApiBackend {
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
            let workspaces =
                tokio::task::spawn_blocking(crate::database::list_workspaces).await??;
            self.exports.republish_workspaces(workspaces);
            Ok(workspace)
        })
    }

    fn published_profile_config(
        &self,
        profile: &str,
    ) -> Option<mj_core::worker_launch::ProfileConfig> {
        self.profile_catalog.published(profile)
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
                let status = match work.await {
                    Ok(Ok(Some(turn_id))) => Some(StartStatus::Submitted { turn_id }),
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

    fn diff(&self, session_id: String) -> BoxFuture<'_, std::result::Result<String, ExportError>> {
        Box::pin(async move {
            self.require_live_target(&session_id)?;
            let layout = export_layout(session_id.clone()).await?;
            let repository = primary_repository_path(&layout)?;
            let mut arguments = vec!["diff".to_owned(), "--repository".to_owned(), repository];
            if let Some(worktree) = &layout.managed_worktree {
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
            let arguments = vec![
                "read-file".to_owned(),
                "--root".to_owned(),
                layout.workspace_root.clone(),
                "--path".to_owned(),
                target_join("", &path).trim_start_matches('/').to_owned(),
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
            let root = primary_repository_path(&layout)?;
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
            let repository = primary_repository_path(&layout)?;
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
                Some(_) => match self.exports.checkpoint_now(session_id.clone()).await {
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
                                    "session {session_id} is busy with a lifecycle operation and \
                                     has no earlier checkpoint to export a bundle from"
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

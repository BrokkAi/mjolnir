//! The daemon half of the documented HTTP API.
//!
//! `mj-controller` serves the `/api/v1` routes but cannot reach the daemon's
//! live session actors or its SQLite store, so it declares the
//! [`SubagentBackend`](crate::server::api::SubagentBackend) trait
//! and this module implements it here, where both are available.
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
use mj_core::subagent::MAX_WAIT_SECONDS;

use crate::quota::ProfileQuota;

use crate::controller::{Controller, SessionExportLayout};
use crate::server::api::{
    BundleExport, ExportError, PushedBranch, StartFollowup, StartStatus, SubagentBackend,
    TranscriptPage, TurnState, TurnSummary,
};
use crate::targets::{self, CancellableProcessExecutor, CommandExecutor, CommandOutput};
use mj_client::session::{BoxFuture, SessionControl, SessionHandle, ViewError, new_command_id};
use mj_core::relay::RelayCommand;

use crate::daemon::RuntimeState;

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
        }
    }

    pub fn with_quota_reports(
        mut self,
        quota_reports: Arc<Mutex<BTreeMap<String, ProfileQuota>>>,
    ) -> Self {
        self.quota_reports = quota_reports;
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
                let config = tokio::task::spawn_blocking(mj_core::config::Config::load).await??;
                let candidates = config
                    .enabled_profiles()
                    .filter(|(id, _)| {
                        config
                            .subagents
                            .profile_is_eligible(&parent.last_profile, id)
                    })
                    .map(|(id, profile)| (id.to_owned(), profile.kind))
                    .collect::<Vec<_>>();
                let ids = {
                    let quota_reports = self
                        .quota_reports
                        .lock()
                        .map_err(|_| anyhow!("sub-agent quota reports lock poisoned"))?;
                    select_profile_per_harness(candidates, &quota_reports)
                };
                let mut profiles = Vec::with_capacity(ids.len());
                for (id, harness) in ids {
                    let choices = self.profile_config(id.clone(), None, false).await?;
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
                if selected_model.is_some() || selected_effort.is_some() {
                    let choices = self
                        .profile_config(profile_id.clone(), selected_model.clone(), false)
                        .await?;
                    crate::server::api::validate_selectors(
                        &choices,
                        selected_model.as_deref(),
                        selected_effort.as_deref(),
                    )
                    .map_err(|failure| anyhow::anyhow!(failure.message))?;
                }
                let ranges = files
                    .iter()
                    .map(|range| crate::server::api::SubagentSourceRange {
                        file: range.file.clone(),
                        start: range.start,
                        end: range.end,
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
                        let (state, _, _) = subagent_status(
                            record.as_ref(),
                            summaries
                                .get(&relation.child_session_id)
                                .and_then(Option::as_ref),
                            starts.get(&relation.child_session_id),
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
                        timeout_seconds.unwrap_or(300).clamp(1, MAX_WAIT_SECONDS),
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
                        subagent_status(record.as_ref(), summary.as_ref(), starts.get(id)).2
                    });
                    if complete || tokio::time::Instant::now() >= deadline {
                        let agents = summaries
                            .into_iter()
                            .map(|(id, summary)| {
                                let record = self.exports.session_record(&id);
                                let (state, output, _) = subagent_status(
                                    record.as_ref(),
                                    summary.as_ref(),
                                    starts.get(&id),
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

    /// Record that a child finished a turn as a Mjolnir notice in the parent's
    /// conversation. This is the one unsolicited sub-agent event, so it is a
    /// notice, not a prompt: it must not forge a user turn or start one. The
    /// child's output is left for `wait_agents` and the child transcript; the
    /// notice only says what happened, and `output` is accepted for the log.
    pub async fn deliver_subagent_completion(
        &self,
        parent_session_id: String,
        child_session_id: &str,
        task_name: &str,
        turn: u64,
        outcome: &str,
        output: &str,
    ) -> Result<()> {
        let _ = output;
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

fn subagent_status(
    record: Option<&mj_core::state::SessionRecord>,
    summary: Option<&mj_core::state::MaterializedSessionSummary>,
    start: Option<&StartStatus>,
) -> (String, Option<String>, bool) {
    if let Some(StartStatus::Failed { message }) = start {
        return ("error".into(), Some(message.clone()), true);
    }
    let start_pending = matches!(start, Some(StartStatus::Pending));
    let lifecycle = record.map(|record| record.state);
    match lifecycle {
        Some(SessionState::Error) => (
            "error".into(),
            record.and_then(|record| record.last_error.clone()),
            true,
        ),
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
            Some(summary) if matches!(summary.execution, MaterializedExecutionState::Idle) => {
                ("completed".into(), summary.last_agent_message.clone(), true)
            }
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
fn still_starting(states: &SessionStateSource, session_id: &str) -> Result<()> {
    match states(session_id) {
        Some(
            SessionState::Provisioning
            | SessionState::Running
            | SessionState::Disconnected
            | SessionState::Checkpointing,
        )
        | None => Ok(()),
        Some(state) => bail!("session {session_id} is {state:?} and will not take a first prompt"),
    }
}

/// Apply model, effort, and the first prompt to a session that is still coming
/// up, returning the turn the prompt was accepted as.
async fn apply_followup(
    sessions: SessionControl,
    states: SessionStateSource,
    session_id: String,
    followup: StartFollowup,
) -> Result<Option<u64>> {
    let deadline = tokio::time::Instant::now() + START_DEADLINE;
    let mut handle = loop {
        still_starting(&states, &session_id)?;
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
        still_starting(&states, &session_id)?;
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

    still_starting(&states, &session_id)?;
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
        turn_start_position: u64,
    ) -> BoxFuture<'_, Result<TurnSummary>> {
        Box::pin(async move {
            blocking("load turn summary", move || {
                crate::database::load_materialized_turn_summary(&session_id, turn_start_position)
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
                    result = apply_followup(sessions, states, followup_id, followup) => result,
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
                Some(_) => {
                    self.exports
                        .checkpoint_now(session_id.clone())
                        .await
                        .map_err(checkpoint_export_error)?
                        .archive_path
                }
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
mod tests {
    use super::*;

    fn quota(profile_id: &str, harness: HarnessKind, remaining: &[u8]) -> ProfileQuota {
        ProfileQuota {
            profile_id: profile_id.into(),
            harness,
            windows: remaining
                .iter()
                .map(|remaining_percent| crate::quota::QuotaWindow {
                    label: "window".into(),
                    remaining_percent: Some(*remaining_percent),
                    used: None,
                    limit: None,
                    resets: None,
                    resets_at_epoch_seconds: None,
                })
                .collect(),
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 0,
        }
    }

    #[test]
    fn subagent_profiles_choose_the_most_remaining_quota_per_harness() {
        let candidates = vec![
            ("codex-low".into(), HarnessKind::Codex),
            ("claude-only".into(), HarnessKind::Claude),
            ("codex-high".into(), HarnessKind::Codex),
        ];
        let reports = BTreeMap::from([
            (
                "codex-low".into(),
                quota("codex-low", HarnessKind::Codex, &[80, 15]),
            ),
            (
                "codex-high".into(),
                quota("codex-high", HarnessKind::Codex, &[60, 55]),
            ),
        ]);

        assert_eq!(
            select_profile_per_harness(candidates, &reports),
            vec![
                ("codex-high".into(), HarnessKind::Codex),
                ("claude-only".into(), HarnessKind::Claude),
            ]
        );
    }

    #[test]
    fn subagent_profile_selection_puts_unknown_quota_last_and_breaks_ties_by_id() {
        let candidates = vec![
            ("codex-unknown".into(), HarnessKind::Codex),
            ("codex-b".into(), HarnessKind::Codex),
            ("codex-a".into(), HarnessKind::Codex),
        ];
        let reports = BTreeMap::from([
            (
                "codex-a".into(),
                quota("codex-a", HarnessKind::Codex, &[50]),
            ),
            (
                "codex-b".into(),
                quota("codex-b", HarnessKind::Codex, &[50]),
            ),
        ]);

        assert_eq!(
            select_profile_per_harness(candidates, &reports),
            vec![("codex-a".into(), HarnessKind::Codex)]
        );
    }

    #[test]
    fn failed_subagent_followup_is_terminal_error_with_its_cause() {
        let status = StartStatus::Failed {
            message: "model is unavailable".into(),
        };
        assert_eq!(
            subagent_status(None, None, Some(&status)),
            ("error".into(), Some("model is unavailable".into()), true)
        );
    }
    use mj_client::session::{
        ManagedSessionView, PendingRelaySubmit, PendingRelaySync, SessionControlBackend,
        SessionHandleBackend,
    };
    use tokio::sync::mpsc;

    /// A session actor that records what it was asked to submit and accepts it
    /// at a fixed ordinal. Hand-written rather than mocked so the test proves
    /// the exact relay command the API sends.
    #[derive(Clone)]
    struct FakeSession {
        session_id: String,
        accepted_ordinal: u64,
        submitted: mpsc::UnboundedSender<(String, RelayCommand)>,
        /// What `view()` reports. `None` is the default empty view, which is
        /// what a session that has not connected yet looks like.
        view: Option<ManagedSessionView>,
    }

    impl SessionHandleBackend for FakeSession {
        fn search_prompts(
            &self,
            _bundle_id: String,
            _scope: mj_core::storage::HistoryScope,
            _query: String,
        ) -> mj_client::session::BoxFuture<'_, Result<Vec<mj_core::storage::PromptHistoryEntry>>>
        {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn review_state(
            &self,
        ) -> mj_client::session::BoxFuture<'_, Result<mj_client::session::ReviewState>> {
            Box::pin(async { Ok(Default::default()) })
        }

        fn config_result(
            &self,
            _command_id: String,
        ) -> BoxFuture<'_, Result<Option<Option<String>>>> {
            Box::pin(async { Ok(Some(None)) })
        }
        fn clone_box(&self) -> Box<dyn SessionHandleBackend> {
            Box::new(self.clone())
        }
        fn session_id(&self) -> &str {
            &self.session_id
        }
        fn view(&self) -> ManagedSessionView {
            self.view.clone().unwrap_or_default()
        }
        fn is_stopped(&self) -> bool {
            false
        }
        fn has_changed(&self) -> Result<bool> {
            Ok(false)
        }
        fn changed(&mut self) -> BoxFuture<'_, Result<ManagedSessionView>> {
            Box::pin(std::future::pending())
        }
        fn enqueue_submit(
            &self,
            command_id: String,
            command: RelayCommand,
        ) -> BoxFuture<'_, Result<PendingRelaySubmit>> {
            let _ = self.submitted.send((command_id, command));
            let ordinal = self.accepted_ordinal;
            Box::pin(async move {
                Ok(PendingRelaySubmit::new(Box::pin(
                    async move { Ok(ordinal) },
                )))
            })
        }
        fn enqueue_sync(&self) -> BoxFuture<'_, Result<PendingRelaySync>> {
            Box::pin(async move { Ok(PendingRelaySync::new(Box::pin(async { Ok(()) }))) })
        }
        fn respond_elicitation(
            &self,
            _elicitation_id: String,
            _response: mj_core::elicitation::ElicitationResponse,
        ) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn stop_background_task(&self, _background_task_id: String) -> BoxFuture<'_, Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn reviewer(
            &self,
            _role: Option<String>,
            _action: mj_client::session::ReviewerAction,
        ) -> BoxFuture<'_, Result<mj_client::session::ReviewerOutcome>> {
            Box::pin(async { anyhow::bail!("no reviewer in this fake") })
        }
    }

    /// Every session this daemon is asked about is up and running.
    fn running_states() -> SessionStateSource {
        Arc::new(|_| Some(SessionState::Running))
    }

    #[test]
    fn checkpoint_export_retains_typed_deferrals_and_real_failures() {
        let error = anyhow!("disk failed");
        assert!(matches!(
            checkpoint_export_error(error),
            ExportError::Failed(_)
        ));
        let error = anyhow::Error::new(crate::controller::CheckpointDeferred::harness_busy())
            .context("capture bundle");
        let ExportError::Refused(message) = checkpoint_export_error(error) else {
            panic!("expected a deferred export");
        };
        assert!(message.contains("capture bundle"));
        assert!(message.contains("agent is working"));
    }

    /// An export runtime with nothing in it. The follow-up tests never export;
    /// the export path's own behavior is proved by the API handler tests.
    struct NoExports;

    impl ExportRuntime for NoExports {
        fn session_record(&self, _session_id: &str) -> Option<mj_core::state::SessionRecord> {
            None
        }
        fn checkpoint_now(
            &self,
            session_id: String,
        ) -> BoxFuture<'_, Result<mj_core::state::CheckpointMetadata>> {
            Box::pin(async move { bail!("session {session_id} cannot be checkpointed in a test") })
        }
    }

    /// A connected view whose harness is ready and offers one model.
    fn ready_view(model: &str) -> ManagedSessionView {
        let materialized = mj_core::state::MaterializedSession::empty("session-1");
        let option: agent_client_protocol::schema::v1::SessionConfigOption =
            serde_json::from_value(serde_json::json!({
                "id": "model",
                "name": "Model",
                "category": "model",
                "type": "select",
                "currentValue": model,
                "options": [{"value": model, "name": model}],
            }))
            .expect("the fixture describes a select the schema accepts");
        let operational = mj_core::relay::RelayOperationalState {
            goal: Default::default(),
            capacity_retry: None,
            activity_turn_started_at_ms: None,
            idle_since_ms: None,
            store_id: None,
            session_id: "session-1".into(),
            execution: mj_core::relay::RelayExecutionState::Idle,
            latest_ordinal: 0,
            latest_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into(),
            acknowledged_through: 0,
            acknowledged_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into(),
            recovery_floor_ordinal: 0,
            recovery_floor_digest: mj_core::relay::RELAY_EVENT_GENESIS_DIGEST.into(),
            native_session_id: Some("native-1".into()),
            native_continuity_lost: false,
            checkpoint_only: false,
            acp_ready: Some(true),
            agent_capabilities: None,
            agent_info: None,
            steering_supported: None,
            config_options: vec![option],
            modes: None,
            available_commands: Vec::new(),
            config: BTreeMap::new(),
            active_prompt: None,
            queued_prompts: Vec::new(),
            active_user_shells: Vec::new(),
            active_agent_terminals: Vec::new(),
            checkpoint_barrier: None,
            checkpoint_ready: None,
            last_acp_activity_at_ms: None,
            current_step_started_at_ms: None,
            foreground_tool_started_at_ms: None,
            harness_turn: None,
            last_harness_turn_started_ordinal: None,
            background_commands: Vec::new(),
            background_work_known: None,
        };
        ManagedSessionView {
            snapshot: Some(mj_core::state::ManagedSessionSnapshot {
                subagent_requests: Vec::new(),
                subagent_results: Vec::new(),
                window: mj_core::state::ProjectionWindow::of(&materialized),
                materialized,
                operational,
                latest_credential_sync_signal: None,
                worker_build: None,
            }),
            connected: true,
            error: None,
        }
    }

    struct FakeControl(FakeSession);

    impl SessionControlBackend for FakeControl {
        fn session(&self, session_id: String) -> BoxFuture<'_, Result<SessionHandle>> {
            let session = self.0.clone();
            Box::pin(async move {
                anyhow::ensure!(session_id == session.session_id, "unknown session");
                Ok(SessionHandle::new(session))
            })
        }
    }

    #[tokio::test]
    async fn prompt_submits_one_text_block_and_returns_its_acceptance_ordinal() {
        let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
        let backend = ApiBackend::new(
            SessionControl::new(FakeControl(FakeSession {
                session_id: "session-1".into(),
                accepted_ordinal: 42,
                submitted: submitted_tx,
                view: None,
            })),
            running_states(),
            Arc::new(NoExports),
        );

        let turn_id = backend
            .prompt("session-1".into(), "add a README line".into())
            .await
            .unwrap();
        assert_eq!(turn_id, 42);

        let (command_id, command) = submitted.recv().await.unwrap();
        assert!(
            command_id.starts_with("api-"),
            "command id {command_id} should name the API as its source"
        );
        let RelayCommand::Prompt { prompt } = command else {
            panic!("the API must submit a prompt command");
        };
        assert_eq!(prompt.len(), 1);
        let ContentBlock::Text(text) = &prompt[0] else {
            panic!("the API must submit the prompt as one text block");
        };
        assert_eq!(text.text, "add a README line");
    }

    #[tokio::test]
    async fn a_finished_subagent_is_recorded_as_a_notice_not_a_prompt() {
        let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
        let backend = ApiBackend::new(
            SessionControl::new(FakeControl(FakeSession {
                session_id: "parent-1".into(),
                accepted_ordinal: 7,
                submitted: submitted_tx,
                view: None,
            })),
            running_states(),
            Arc::new(NoExports),
        );

        backend
            .deliver_subagent_completion(
                "parent-1".into(),
                "child-abcdef012345",
                "audit deps",
                3,
                "completed",
                "the full child output that must not be pasted into the notice",
            )
            .await
            .unwrap();

        let (command_id, command) = submitted.recv().await.unwrap();
        assert!(
            command_id.starts_with("subagent-"),
            "notice command id {command_id} should name the sub-agent path"
        );
        let RelayCommand::RecordNotice { text } = command else {
            panic!("a finished sub-agent must be a notice, not {command:?}");
        };
        assert!(
            text.contains("audit deps") && text.contains("finished turn 3"),
            "unexpected notice text: {text}"
        );
        assert!(
            !text.contains("full child output"),
            "the notice must stay terse, not paste the child output: {text}"
        );
    }

    #[tokio::test]
    async fn the_start_follow_up_configures_the_model_before_it_prompts() {
        let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
        let backend = ApiBackend::new(
            SessionControl::new(FakeControl(FakeSession {
                session_id: "session-1".into(),
                accepted_ordinal: 12,
                submitted: submitted_tx,
                view: Some(ready_view("gpt-5-codex")),
            })),
            running_states(),
            Arc::new(NoExports),
        );

        backend
            .start_followup(
                "session-1".into(),
                StartFollowup {
                    model: Some("gpt-5-codex".into()),
                    effort: None,
                    prompt: Some("add a README line".into()),
                },
            )
            .await
            .unwrap();

        let (_, first) = submitted.recv().await.unwrap();
        assert_eq!(
            first,
            RelayCommand::SetConfig {
                key: "model".into(),
                value: "gpt-5-codex".into(),
            },
            "the model must be set before the prompt runs under the old one"
        );
        let (_, second) = submitted.recv().await.unwrap();
        assert!(matches!(second, RelayCommand::Prompt { .. }));

        // The status the follow-up records is what a wait blocks on, so it has
        // to name the turn the prompt was accepted as.
        let status = loop {
            match backend.start_status("session-1".into()).await.unwrap() {
                Some(StartStatus::Pending) | None => tokio::task::yield_now().await,
                Some(status) => break status,
            }
        };
        assert_eq!(status, StartStatus::Submitted { turn_id: 12 });
    }

    #[tokio::test]
    async fn the_start_follow_up_refuses_a_model_the_agent_does_not_offer() {
        let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
        let backend = ApiBackend::new(
            SessionControl::new(FakeControl(FakeSession {
                session_id: "session-1".into(),
                accepted_ordinal: 12,
                submitted: submitted_tx,
                view: Some(ready_view("gpt-5-codex")),
            })),
            running_states(),
            Arc::new(NoExports),
        );

        backend
            .start_followup(
                "session-1".into(),
                StartFollowup {
                    model: Some("no-such-model".into()),
                    effort: None,
                    prompt: Some("add a README line".into()),
                },
            )
            .await
            .unwrap();

        let status = loop {
            match backend.start_status("session-1".into()).await.unwrap() {
                Some(StartStatus::Pending) | None => tokio::task::yield_now().await,
                Some(status) => break status,
            }
        };
        let StartStatus::Failed { message } = status else {
            panic!("an unavailable model must fail the start, not submit the prompt");
        };
        assert!(message.contains("no-such-model"), "unexpected: {message}");
        assert!(
            submitted.try_recv().is_err(),
            "nothing may be submitted once the configuration is refused"
        );
        backend
            .set_config("session-1".into(), "model".into(), "gpt-5-codex".into())
            .await
            .unwrap();
        assert!(
            backend
                .start_status("session-1".into())
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            submitted.recv().await.unwrap().1,
            RelayCommand::SetConfig { .. }
        ));
        assert!(
            submitted.try_recv().is_err(),
            "repair must not replay the abandoned initial prompt"
        );
        assert_eq!(
            backend
                .prompt("session-1".into(), "repaired prompt".into())
                .await
                .unwrap(),
            12
        );
        assert!(matches!(
            submitted.recv().await.unwrap().1,
            RelayCommand::Prompt { .. }
        ));
    }

    #[tokio::test]
    async fn closing_cancels_the_supervised_start_before_any_prompt() {
        let (submitted_tx, mut submitted) = mpsc::unbounded_channel();
        let backend = ApiBackend::new(
            SessionControl::new(FakeControl(FakeSession {
                session_id: "session-1".into(),
                accepted_ordinal: 1,
                submitted: submitted_tx,
                view: None,
            })),
            running_states(),
            Arc::new(NoExports),
        );
        backend
            .start_followup(
                "session-1".into(),
                StartFollowup {
                    prompt: Some("must not run".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let task = backend
            .starts
            .lock()
            .unwrap()
            .get_mut("session-1")
            .unwrap()
            .task
            .take()
            .unwrap();
        backend.cancel_start("session-1".into()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(submitted.try_recv().is_err());
        assert!(
            backend
                .start_status("session-1".into())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn prompt_reports_a_session_the_manager_does_not_hold() {
        let (submitted_tx, _submitted) = mpsc::unbounded_channel();
        let backend = ApiBackend::new(
            SessionControl::new(FakeControl(FakeSession {
                session_id: "session-1".into(),
                accepted_ordinal: 1,
                submitted: submitted_tx,
                view: None,
            })),
            running_states(),
            Arc::new(NoExports),
        );
        let error = backend
            .prompt("session-2".into(), "hello".into())
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("session-2 is not running"),
            "unexpected error: {error:#}"
        );
    }
}

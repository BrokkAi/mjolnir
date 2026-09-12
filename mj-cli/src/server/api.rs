//! The daemon half of the documented HTTP API.
//!
//! `mj-controller` serves the `/api/v1` routes but cannot reach the daemon's
//! live session actors or its SQLite store, so it declares the
//! [`SubagentBackend`](mj_controller::hel_server::api::SubagentBackend) trait
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

use hel::hel_state::{MaterializedExecutionState, SessionState};
use hel::hel_targets::{self, CancellableProcessExecutor, CommandExecutor, CommandOutput};
use hel::hel_worker::RelayCommand;
use mj_client::session::{BoxFuture, SessionControl, SessionHandle, ViewError, new_command_id};
use mj_controller::hel_controller::{Controller, SessionExportLayout};
use mj_controller::hel_server::api::{
    BundleExport, ExportError, PushedBranch, StartFollowup, StartStatus, SubagentBackend,
    TranscriptPage, TurnState, TurnSummary,
};

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
    fn session_record(&self, session_id: &str) -> Option<hel::hel_state::SessionRecord>;

    /// Checkpoint a session now, returning the archive it wrote.
    fn checkpoint_now(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<hel::hel_state::CheckpointMetadata>>;
}

impl ExportRuntime for RuntimeState {
    fn session_record(&self, session_id: &str) -> Option<hel::hel_state::SessionRecord> {
        RuntimeState::session_record(self, session_id)
    }

    fn checkpoint_now(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<hel::hel_state::CheckpointMetadata>> {
        Box::pin(async move { self.checkpoint_session_now(&session_id).await })
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
                task.abort();
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
    let snapshot = loop {
        let view = handle.view();
        if let Some(ViewError::TargetMissing(detail)) = &view.error {
            bail!("session {session_id} lost its target: {detail}");
        }
        match &view.snapshot {
            Some(snapshot)
                if view.connected
                    && (!needs_config || snapshot.operational.native_session_is_ready()) =>
            {
                break snapshot.clone();
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
    };

    for (key, value) in [("model", followup.model), ("effort", followup.effort)] {
        let Some(value) = value else {
            continue;
        };
        let choices =
            hel::hel_acp::session_config_choices(&snapshot.operational.config_options, key);
        ensure!(
            choices.iter().any(|choice| choice.value == value),
            "this agent does not offer {value} as a {key}"
        );
        handle
            .submit(
                new_command_id("api-set-config")?,
                RelayCommand::SetConfig {
                    key: key.to_owned(),
                    value,
                },
            )
            .await?;
    }

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
            hel_targets::worker_root(&layout.backend, &session_id)?
        );
        let mut argv = vec![binary, "worker".to_owned()];
        argv.extend(arguments);
        let command = hel_targets::command_on_locator(&layout.backend, &session_id, argv, purpose)?;
        CancellableProcessExecutor::with_timeout(EXPORT_TIMEOUT).execute(&command)
    })
    .await?;
    match output.status {
        0 => Ok(output.stdout),
        // A worker installed before these subcommands existed answers clap's
        // usage failure. That is not a failed export: resuming the session
        // reinstalls the worker and the same call then works.
        CLAP_USAGE_EXIT_CODE => Err(ExportError::Refused(
            "worker on this target predates export support; resume the session to upgrade".into(),
        )),
        // The worker met a precondition it could not satisfy — no recorded
        // base, no push remote, a path outside the workspace — and printed the
        // reason. That is the caller's to fix, so it is a refusal, not a
        // failed export.
        hel::hel_archive::EXPORT_REFUSED_EXIT_CODE => Err(ExportError::Refused(refusal_reason(
            &output.stderr,
            purpose,
        ))),
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
    fn session_handle(&self, session_id: String) -> BoxFuture<'_, Result<Option<SessionHandle>>> {
        Box::pin(async move { Ok(self.sessions.session(session_id).await.ok()) })
    }

    fn prompt(&self, session_id: String, text: String) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move {
            let handle = self
                .sessions
                .session(session_id.clone())
                .await
                .with_context(|| format!("session {session_id} is not running"))?;
            submit_prompt(&handle, text).await
        })
    }

    fn turn_state(&self, session_id: String) -> BoxFuture<'_, Result<Option<TurnState>>> {
        Box::pin(async move {
            blocking("load turn outcome", move || {
                Ok(
                    hel::hel_database::load_materialized_turn_outcome(&session_id)?.map(
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
                hel::hel_database::load_materialized_turn_summary(&session_id, turn_start_position)
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
                    },
                );
            let work = tokio::spawn(apply_followup(sessions, states, id.clone(), followup));
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

    fn lookup_idempotency(&self, key: String) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async move {
            blocking("look up idempotency key", move || {
                hel::hel_database::lookup_api_idempotency(&key)
            })
            .await
        })
    }

    fn record_idempotency(&self, key: String, session_id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            blocking("record idempotency key", move || {
                hel::hel_database::record_api_idempotency(&key, &session_id)
            })
            .await
        })
    }

    fn transcript(
        &self,
        session_id: String,
        after_seq: u64,
        limit: usize,
    ) -> BoxFuture<'_, Result<Option<TranscriptPage>>> {
        Box::pin(async move {
            blocking("load transcript page", move || {
                hel::hel_database::load_materialized_transcript_after(&session_id, after_seq, limit)
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
                "--repository".to_owned(),
                repository,
                "--branch".to_owned(),
                branch,
            ];
            let stdout =
                worker_command(layout, session_id, arguments, "session branch push").await?;
            let pushed: hel::hel_archive::PushedBranch =
                serde_json::from_slice(&stdout).map_err(|error| {
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
                        .map_err(ExportError::Failed)?
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
                hel::hel_archive::verify_repository_bundles_streaming(&archive_path)
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
            _response: hel::hel_elicitation::ElicitationResponse,
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

    /// An export runtime with nothing in it. The follow-up tests never export;
    /// the export path's own behavior is proved by the API handler tests.
    struct NoExports;

    impl ExportRuntime for NoExports {
        fn session_record(&self, _session_id: &str) -> Option<hel::hel_state::SessionRecord> {
            None
        }
        fn checkpoint_now(
            &self,
            session_id: String,
        ) -> BoxFuture<'_, Result<hel::hel_state::CheckpointMetadata>> {
            Box::pin(async move { bail!("session {session_id} cannot be checkpointed in a test") })
        }
    }

    /// A connected view whose harness is ready and offers one model.
    fn ready_view(model: &str) -> ManagedSessionView {
        let materialized = hel::hel_state::MaterializedSession::empty("session-1");
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
        let operational = hel::hel_worker::RelayOperationalState {
            capacity_retry: None,
            activity_turn_started_at_ms: None,
            idle_since_ms: None,
            store_id: None,
            session_id: "session-1".into(),
            execution: hel::hel_worker::RelayExecutionState::Idle,
            latest_ordinal: 0,
            latest_digest: hel::hel_worker::RELAY_EVENT_GENESIS_DIGEST.into(),
            acknowledged_through: 0,
            acknowledged_digest: hel::hel_worker::RELAY_EVENT_GENESIS_DIGEST.into(),
            recovery_floor_ordinal: 0,
            recovery_floor_digest: hel::hel_worker::RELAY_EVENT_GENESIS_DIGEST.into(),
            native_session_id: Some("native-1".into()),
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
            snapshot: Some(hel::hel_state::ManagedSessionSnapshot {
                window: hel::hel_state::ProjectionWindow::of(&materialized),
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

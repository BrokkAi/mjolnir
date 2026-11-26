use super::*;

/// A streaming upload body whose blocking `Read` end can be woken and closed
/// immediately when cancellation starts or the target subprocess exits early.
pub struct UploadBody {
    reader: Box<dyn std::io::Read + Send>,
    abort: Arc<dyn Fn() + Send + Sync>,
}

impl UploadBody {
    pub fn new(reader: Box<dyn std::io::Read + Send>, abort: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self { reader, abort }
    }

    pub fn into_parts(self) -> (Box<dyn std::io::Read + Send>, Arc<dyn Fn() + Send + Sync>) {
        (self.reader, self.abort)
    }
}

/// Everything the API needs from the daemon: live session actors, the durable
/// projection, and the target-side git operations.
///
/// The daemon's implementation lives in `server_runtime::api`; route tests
/// supply a fake.
pub trait SubagentBackend: Send + Sync {
    fn dashboard_backend(&self) -> Option<Arc<dyn DashboardBackend>> {
        None
    }

    fn create_bundle(
        &self,
        _source: String,
        _exact_sources: Option<Vec<String>>,
        _discovered: bool,
        _include_project: bool,
        _admission: mj_core::upgrade::Work,
    ) -> BoxFuture<'_, Result<Result<CreateBundleResponse, BundleFailure>, ApiFailure>> {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn preflight_new(
        &self,
        _request: mj_client::daemon::NewSessionPreflightRequest,
        _direct: bool,
        _admission: Option<mj_core::upgrade::Work>,
        _shutdown: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'_, Result<Result<PreflightNew, PreflightFailure>, ApiFailure>> {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn preflight_resume(
        &self,
        _session_id: String,
        _target_id: String,
    ) -> BoxFuture<'_, Result<Result<PreflightResume, PreflightFailure>, ApiFailure>> {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn complete_path(
        &self,
        _host: CompletionHost,
        _prefix: String,
        _kind: CompletionKind,
    ) -> BoxFuture<'_, Result<Result<PathCompletion, String>, ApiFailure>> {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn discover_projects(
        &self,
        _request: mj_import::project_picker::ProjectDiscoveryRequest,
    ) -> BoxFuture<
        '_,
        Result<Result<mj_import::project_picker::ProjectDiscovery, &'static str>, ApiFailure>,
    > {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn project_catalog(
        &self,
        _refresh: bool,
        _retry: bool,
    ) -> BoxFuture<
        '_,
        Result<Result<mj_core::project_catalog::ProjectCatalogView, String>, ApiFailure>,
    > {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn prepare_move(
        &self,
        _selection: MoveSelection,
        _direct: bool,
    ) -> BoxFuture<'_, Result<Result<MovePreparation, Option<Refusal>>, ApiFailure>> {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn mark_conversation_read(
        &self,
        _client_id: String,
        _session_id: String,
        _through: u64,
    ) -> BoxFuture<'_, Result<Result<(), String>, ApiFailure>> {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn client_state(
        &self,
        _client_id: String,
        _session_id: String,
    ) -> BoxFuture<'_, Result<Result<ViewerClientState, String>, ApiFailure>> {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn save_draft(
        &self,
        _client_id: String,
        _session_id: String,
        _draft: String,
    ) -> BoxFuture<'_, Result<Result<(), String>, ApiFailure>> {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn mark_workspace_read(
        &self,
        _client_id: String,
        _workspace_id: String,
    ) -> BoxFuture<'_, Result<Result<(), String>, ApiFailure>> {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn prompt_history(
        &self,
        _session_id: String,
        _query: String,
        _scope: String,
    ) -> BoxFuture<'_, Result<Result<ViewerPromptHistory, String>, ApiFailure>> {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn set_container_settings(
        &self,
        _session_id: String,
        _resource_allocation: Option<SessionResourceAllocation>,
        _mounts: Vec<AdditionalMount>,
    ) -> BoxFuture<
        '_,
        Result<Result<Option<mj_core::state::ContainerUpgradePreview>, ActionOutcome>, ApiFailure>,
    > {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn container_upgrade_preview(
        &self,
        _session_id: String,
    ) -> BoxFuture<'_, Result<Result<mj_core::state::ContainerUpgradePreview, String>, ApiFailure>>
    {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn resolve_suspend_blockers(
        &self,
        _session_id: String,
    ) -> BoxFuture<
        '_,
        Result<
            Result<mj_core::suspend_consent::ResolveSuspendBlockersResponse, String>,
            ApiFailure,
        >,
    > {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }
    fn suspend_preflight(
        &self,
        _session_id: String,
        _dry_run: bool,
        _without_checkpoint: bool,
        _history_only: bool,
    ) -> BoxFuture<'_, Result<Result<mj_core::suspend_consent::SuspendPreflight, String>, ApiFailure>>
    {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn purge_untracked(
        &self,
        _request: PurgeUntrackedRequest,
    ) -> BoxFuture<'_, Result<Result<PurgeUntrackedResponse, PurgeUntrackedFailure>, ApiFailure>>
    {
        Box::pin(async { Err(ApiFailure::unavailable("the controller is unavailable")) })
    }

    fn stop_background_task(
        &self,
        _session_id: String,
        _background_task_id: String,
    ) -> BoxFuture<'_, Result<Result<(), BackgroundTaskStopFailure>, ApiFailure>> {
        Box::pin(async { Err(ApiError::controller_unavailable().into()) })
    }

    fn push_notice(&self, _session_id: &str, _message: String) {}
    /// Current login failures from the daemon's publication; memory only.
    fn profile_login_errors(&self) -> AnyResult<std::collections::BTreeMap<String, String>> {
        Ok(Default::default())
    }

    fn subagent_config(&self) -> BoxFuture<'_, AnyResult<mj_core::config::SubagentConfig>> {
        Box::pin(async { anyhow::bail!("sub-agent configuration unavailable") })
    }

    /// The dashboard's load-balancing preference order, from `[quota] prefer`.
    fn quota_preferences(&self) -> BoxFuture<'_, AnyResult<Vec<String>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Current trailing-60-minute velocity and durable placement reservations.
    fn quota_telemetry(
        &self,
    ) -> BoxFuture<'_, AnyResult<mj_accounts::quota_telemetry::QuotaTelemetrySnapshot>> {
        Box::pin(async { Ok(Default::default()) })
    }

    fn quota_telemetry_store(&self) -> mj_accounts::quota_telemetry::QuotaTelemetryStore {
        mj_accounts::quota_telemetry::QuotaTelemetryStore::new()
    }

    /// Current power-ranking snapshot (compiled baseline or cached fetch).
    fn power_ranking(
        &self,
    ) -> BoxFuture<'_, AnyResult<mj_accounts::power_ranking::PowerRankingSnapshot>> {
        Box::pin(async { Ok(mj_accounts::power_ranking::PowerRankingSnapshot::baseline().clone()) })
    }

    /// Record an accepted placement in the daemon's in-memory telemetry so
    /// burst spawns before the next 10-minute poll see its reservation.
    fn record_placement(&self, _profile_id: &str, _category: &str) {}

    /// Cheap token for this session's durable wait inputs. Backends without
    /// a committed publication leave filtering to the detailed observation.
    fn wait_revision(&self, _session_id: &str) -> AnyResult<Option<u64>> {
        Ok(None)
    }

    /// Confirm before session admission that the configured bundle selects at
    /// most one GitHub App installation.
    fn validate_github_bundle(
        &self,
        _bundle_id: String,
    ) -> BoxFuture<'_, Result<(), mj_session_runtime::github_app::GithubBundleSelectionError>> {
        Box::pin(async { Ok(()) })
    }

    /// Return one currently valid token for an owner installation or a set of
    /// repositories that resolve to one installation.
    fn github_token(
        &self,
        _owner: Option<String>,
        _repositories: Vec<(String, String)>,
    ) -> BoxFuture<'_, AnyResult<String>> {
        Box::pin(async {
            anyhow::bail!(
                "GitHub App credentials are not configured; set [github.app] in config.toml"
            )
        })
    }

    fn transcript_history(
        &self,
        session_id: String,
        before: Option<mj_core::storage::TranscriptCursor>,
    ) -> BoxFuture<'_, AnyResult<mj_core::storage::TranscriptHistoryPage>> {
        Box::pin(async move {
            mj_core::runtime::spawn_blocking(move || {
                mj_store::database::load_transcript_history(&session_id, before.as_ref(), 128)?
                    .context("session history is unavailable")
            })
            .await?
        })
    }

    fn native_agent_tool_call_detail(
        &self,
        owner: String,
        child: String,
        stable_id: String,
    ) -> BoxFuture<'_, AnyResult<Option<mj_core::transcript::ToolCallDetailResult>>> {
        Box::pin(async move {
            mj_core::runtime::spawn_blocking(move || {
                mj_store::database::load_native_agent_tool_call_detail(&owner, &child, &stable_id)
            })
            .await?
        })
    }

    fn tool_call_detail(
        &self,
        session_id: String,
        stable_id: String,
    ) -> BoxFuture<'_, AnyResult<Option<mj_core::transcript::ToolCallDetailResult>>> {
        Box::pin(async move {
            mj_core::runtime::spawn_blocking(move || {
                mj_session_runtime::tool_detail::load_tool_call_detail_with_checkpoint_fallback(
                    &session_id,
                    &stable_id,
                )
            })
            .await?
        })
    }

    fn native_agent_history(
        &self,
        owner: String,
        child: String,
        before: Option<(u64, String)>,
    ) -> BoxFuture<'_, AnyResult<mj_core::native_agent::NativeAgentHistoryPage>> {
        Box::pin(async move {
            mj_core::runtime::spawn_blocking(move || {
                mj_store::database::native_agent_history(&owner, &child, before)
            })
            .await?
        })
    }
    fn events(
        &self,
        filter: mj_store::database::ApiEventFilter,
        after_seq: Option<u64>,
    ) -> BoxFuture<'_, AnyResult<mj_store::database::ApiEventPage>> {
        events::load_events(filter, after_seq)
    }

    fn profile_config(
        &self,
        profile: String,
        model: Option<String>,
        refresh: bool,
    ) -> BoxFuture<'_, AnyResult<mj_core::worker_launch::ProfileConfig>> {
        Box::pin(mj_profile_catalog::profile_config::discover(
            profile, model, refresh,
        ))
    }

    /// What `profile`'s harness advertises before the profile's `models`
    /// allow-list, for `mj doctor`. Backends without the daemon's catalogue
    /// answer with the ordinary choices.
    fn unrestricted_profile_config(
        &self,
        profile: String,
    ) -> BoxFuture<'_, AnyResult<mj_core::worker_launch::ProfileConfig>> {
        self.profile_config(profile, None, false)
    }
    /// Start discovering what `config` selects, such as a Settings draft's
    /// sub-agent models. Results arrive through the capability publication.
    fn warm_profile_capabilities(&self, _config: &mj_core::config::Config) {}
    fn profile_capabilities_snapshot(
        &self,
    ) -> Option<mj_core::profile_capabilities::ProfileCapabilitiesSnapshot> {
        None
    }
    /// Every enabled profile a parent running on `parent_profile` may use for
    /// a sub-agent, with its discovered choices and remaining quota. It waits
    /// for pending discovery; a profile whose discovery fails is reported as
    /// unavailable rather than failing the whole answer.
    fn subagent_candidates(
        &self,
        _parent_profile: String,
    ) -> BoxFuture<'_, AnyResult<SubagentCandidates>> {
        Box::pin(async { anyhow::bail!("sub-agent profiles are unavailable") })
    }
    /// Enabled profiles available to review children, with current quota and health.
    fn reviewer_candidates(&self) -> BoxFuture<'_, AnyResult<SubagentCandidates>> {
        Box::pin(async { anyhow::bail!("reviewer profiles are unavailable") })
    }
    /// Waits for a profile not in `tried` to report that it offers `model`,
    /// then returns the ready candidates that were not tried. A profile whose
    /// capability check fails is reported in `unavailable` with its reason,
    /// and not tried again. It fails when no further profile does and none
    /// could be checked, or when no profile returned a capability list.
    fn reviewer_candidates_for_model(
        &self,
        _model: String,
        _tried: Vec<String>,
    ) -> BoxFuture<'_, AnyResult<SubagentCandidates>> {
        self.reviewer_candidates()
    }
    fn reviewer_autoconfigure_candidates(&self) -> BoxFuture<'_, AnyResult<SubagentCandidates>> {
        self.reviewer_candidates()
    }
    /// The efforts `profile` offers for `model` from completed discovery,
    /// without starting a probe; `None` is unknown.
    fn cached_model_efforts(
        &self,
        _profile: &str,
        _model: &str,
    ) -> Option<Vec<mj_core::acp::SessionConfigChoice>> {
        None
    }
    fn reviewer_ready_candidates(
        &self,
        _model: String,
    ) -> BoxFuture<'_, AnyResult<SubagentCandidates>> {
        self.reviewer_candidates()
    }
    /// Every enabled profile usable for a user-created session, with its
    /// discovered choices and remaining quota. This is independent of the
    /// enabled profile set available for sub-agent use.
    fn session_profile_candidates(&self) -> BoxFuture<'_, AnyResult<SubagentCandidates>> {
        Box::pin(async { anyhow::bail!("session profile selection is unavailable") })
    }
    fn start_subagent(
        &self,
        _request: mj_session_types::RegisterSubagentRequest,
    ) -> BoxFuture<'_, AnyResult<mj_core::subagent::SubagentRecord>> {
        Box::pin(async { anyhow::bail!("sub-agent creation is unavailable") })
    }
    fn list_subagents(
        &self,
        parent_session_id: String,
    ) -> BoxFuture<'_, AnyResult<Vec<mj_core::subagent::SubagentRecord>>> {
        Box::pin(async move {
            mj_core::runtime::spawn_blocking(move || {
                mj_store::database::list_subagents(&parent_session_id)
            })
            .await?
        })
    }
    /// Whether a sub-agent child has handed back its report for its parent's
    /// newest task; see [`mj_store::database::subagent_has_handed_back`].
    fn subagent_handed_back(&self, child_session_id: String) -> BoxFuture<'_, AnyResult<bool>> {
        Box::pin(async move {
            let liveness = self.subagent_work_liveness(child_session_id.clone()).await;
            mj_core::runtime::spawn_blocking(move || {
                mj_store::database::subagent_has_handed_back(&child_session_id, &liveness)
            })
            .await?
        })
    }
    /// Whether the processes a child's last turn left are still alive, as its
    /// worker published it. The store does not carry it, so a backend without
    /// live sessions publishes none.
    fn subagent_work_liveness(
        &self,
        _child_session_id: String,
    ) -> BoxFuture<'_, mj_core::subagent::Liveness> {
        Box::pin(async { mj_core::subagent::Liveness::Known(None) })
    }
    fn read_context_file(
        &self,
        session_id: String,
        path: PathBuf,
    ) -> BoxFuture<'_, std::result::Result<Vec<u8>, ExportError>> {
        Box::pin(async move {
            let fetched = self
                .read_file(session_id, path, mj_checkpoint::archive::FetchMode::Content)
                .await?;
            match fetched.kind {
                mj_checkpoint::archive::FetchedKind::File => {
                    mj_core::runtime::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
                        let mut bytes = Vec::new();
                        std::io::Read::read_to_end(&mut fetched.reader(), &mut bytes)?;
                        Ok(bytes)
                    })
                    .await
                    .map_err(|error| ExportError::Failed(error.into()))?
                    .map_err(ExportError::Failed)
                }
                _ => Err(ExportError::Refused(format!(
                    "{} is a directory, not a file",
                    fetched.path
                ))),
            }
        })
    }
    /// The workspaces the store holds, in the order the terminal's tabs and the
    /// viewer's list show them.
    fn list_workspaces(
        &self,
    ) -> BoxFuture<'_, AnyResult<Vec<mj_core::workspace::WorkspaceRecord>>> {
        Box::pin(async {
            mj_core::runtime::spawn_blocking(mj_store::database::list_workspaces).await?
        })
    }
    /// One listed workspace, read from the store. `None` means the list
    /// would not contain it. Action validation asks this instead of the
    /// published copy of the list, which can lag behind the store (#1334).
    fn workspace(
        &self,
        id: String,
    ) -> BoxFuture<'_, AnyResult<Option<mj_core::workspace::WorkspaceRecord>>> {
        Box::pin(async move {
            mj_core::runtime::spawn_blocking(move || mj_store::database::workspace_record(&id))
                .await?
        })
    }
    /// The workspace with this name, creating it when the store holds none.
    ///
    /// This is the daemon's own `CreateWorkspace` operation: create-or-get, so
    /// two callers that both saw an empty list attach to the same normalized
    /// name instead of one of them meeting a SQLite conflict. The daemon
    /// overrides it to republish the list afterwards.
    fn create_workspace(
        &self,
        name: String,
    ) -> BoxFuture<'_, AnyResult<mj_core::workspace::WorkspaceRecord>> {
        Box::pin(async move {
            mj_core::runtime::spawn_blocking(move || {
                mj_store::database::create_or_get_workspace(&name)
            })
            .await?
        })
    }
    fn refresh_workspaces(&self) -> BoxFuture<'_, AnyResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn close_workspace(
        &self,
        _id: String,
        _admission: mj_core::upgrade::Work,
    ) -> BoxFuture<'_, Result<(), ApiFailure>> {
        Box::pin(async { Err(ApiFailure::unavailable("workspace close is unavailable")) })
    }

    fn cancel_workspace_close(&self, _id: &str) -> Result<(), ApiFailure> {
        Err(ApiFailure::unavailable(
            "workspace close cancellation is unavailable",
        ))
    }

    fn list_workspace_drafts(
        &self,
    ) -> BoxFuture<'_, AnyResult<Vec<mj_client::daemon::WorkspaceDrafts>>> {
        Box::pin(async { anyhow::bail!("terminal drafts are unavailable") })
    }

    fn remove_project(&self, _id: String) -> BoxFuture<'_, AnyResult<()>> {
        Box::pin(async { anyhow::bail!("project removal is unavailable") })
    }

    fn set_config(
        &self,
        session_id: String,
        key: String,
        value: String,
    ) -> BoxFuture<'_, AnyResult<()>> {
        Box::pin(async move {
            self.session_handle(session_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("session has no live actor"))?
                .set_config(key, value)
                .await
        })
    }
    fn cancel_start(&self, _session_id: String) -> BoxFuture<'_, AnyResult<()>> {
        Box::pin(async { Ok(()) })
    }
    /// The live actor for a session, or `None` when none holds it.
    fn session_handle(&self, session_id: String)
    -> BoxFuture<'_, AnyResult<Option<SessionHandle>>>;

    /// Submit a prompt, returning its relay acceptance ordinal.
    fn prompt(&self, session_id: String, text: String) -> BoxFuture<'_, AnyResult<u64>>;

    /// The full session ID a message from the user names: the ID itself, or
    /// the one session whose ID starts with this hexadecimal prefix.
    fn resolve_message_target(&self, _requested_id: String) -> BoxFuture<'_, AnyResult<String>> {
        Box::pin(async { anyhow::bail!("session messaging is unavailable") })
    }

    /// Deliver one session's message to another session, or to one of its
    /// own sub-agents, using the daemon's shared authorization and
    /// mailbox/turn routing operation. A message from the user is a prompt
    /// and does not come here.
    fn deliver_message(
        &self,
        _sender_session_id: String,
        _target_session_id: String,
        _text: String,
        _request_id: String,
        _created_at_ms: i64,
    ) -> BoxFuture<'_, AnyResult<SessionMessageResponse>> {
        Box::pin(async { anyhow::bail!("session messaging is unavailable") })
    }

    /// Record a prompt a person is about to send as `command_id` in their
    /// prompt history. Agent messages, which also use
    /// [`Self::prompt_with_id`], are not recorded.
    fn record_user_prompt(
        &self,
        _session_id: String,
        _command_id: String,
        _text: String,
    ) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    /// Submit under a producer-owned identity, preserving the receipt on retries.
    fn prompt_with_id(
        &self,
        session_id: String,
        text: String,
        command_id: Option<String>,
    ) -> BoxFuture<'_, AnyResult<u64>> {
        let Some(command_id) = command_id else {
            return self.prompt(session_id, text);
        };
        Box::pin(async move {
            let handle = self
                .session_handle(session_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("session has no live worker"))?;
            handle
                .submit(
                    command_id,
                    mj_core::relay::RelayCommand::Prompt {
                        prompt: vec![agent_client_protocol::v1::schema::ContentBlock::Text(
                            agent_client_protocol::v1::schema::TextContent::new(text),
                        )],
                    },
                )
                .await
        })
    }

    /// Durable turn state for a session with no live actor.
    fn turn_state(&self, session_id: String) -> BoxFuture<'_, AnyResult<Option<TurnState>>>;

    /// A sub-agent child's recorded report, and whether it has the handback
    /// tool. `None` for a session that is not a Mjolnir sub-agent.
    fn subagent_report(
        &self,
        _session_id: String,
    ) -> BoxFuture<'_, AnyResult<Option<(bool, mj_core::subagent::SubagentReport)>>> {
        Box::pin(async { Ok(None) })
    }

    /// Summarize the turn that covers these transcript positions.
    fn turn_summary(
        &self,
        session_id: String,
        turn: TurnSpan,
    ) -> BoxFuture<'_, AnyResult<TurnSummary>>;

    /// Apply model, effort, and the first prompt once a new session is ready.
    fn start_followup(
        &self,
        session_id: String,
        followup: StartFollowup,
    ) -> BoxFuture<'_, AnyResult<()>>;

    /// How far a created session's follow-up has got.
    fn start_status(&self, session_id: String) -> BoxFuture<'_, AnyResult<Option<StartStatus>>>;

    /// A page of transcript items after `after_seq`.
    fn transcript(
        &self,
        session_id: String,
        after_seq: u64,
        limit: usize,
        roles: Vec<mj_core::transcript::TranscriptRole>,
        finished_only: bool,
    ) -> BoxFuture<'_, AnyResult<Option<TranscriptPage>>>;

    fn usage(
        &self,
        session_id: String,
        after_seq: u64,
        limit: usize,
    ) -> BoxFuture<'_, AnyResult<Option<mj_store::database::UsagePage>>> {
        Box::pin(async move {
            mj_core::runtime::spawn_blocking(move || {
                mj_store::database::load_session_usage(&session_id, after_seq, limit)
            })
            .await?
        })
    }

    /// One finished review's per-role output, or the latest when `review_id`
    /// is `None`. The rows outlive the reviewer child sessions.
    fn finished_review(
        &self,
        session_id: String,
        review_id: Option<String>,
    ) -> BoxFuture<'_, AnyResult<Option<mj_store::database::FinishedReview>>> {
        Box::pin(async move {
            mj_core::runtime::spawn_blocking(move || {
                mj_store::database::finished_review(&session_id, review_id.as_deref())
            })
            .await?
        })
    }

    fn usage_tree(
        &self,
        parent: String,
    ) -> BoxFuture<'_, AnyResult<Option<mj_store::database::UsageTree>>> {
        Box::pin(async move {
            mj_core::runtime::spawn_blocking(move || mj_store::database::load_usage_tree(&parent))
                .await?
        })
    }

    fn usage_report(
        &self,
        query: mj_core::usage::SessionUsageQuery,
    ) -> BoxFuture<'_, AnyResult<mj_core::usage::SessionUsageReport>> {
        Box::pin(async move {
            let models = mj_profile_catalog::profile_config::advertised_models();
            mj_core::runtime::spawn_blocking(move || {
                mj_store::database::load_session_usage_report(&query, models)
            })
            .await?
        })
    }

    /// A unified diff of the session's work.
    fn diff(
        &self,
        session_id: String,
        options: DiffOptions,
    ) -> BoxFuture<'_, Result<String, ExportError>>;

    /// One file, a directory as a zip archive, or a listing of either, from
    /// the session's target.
    fn read_file(
        &self,
        session_id: String,
        path: PathBuf,
        mode: mj_checkpoint::archive::FetchMode,
    ) -> BoxFuture<'_, Result<mj_checkpoint::archive::FetchedPath, ExportError>>;

    fn write_file(
        &self,
        _session_id: String,
        _path: PathBuf,
        _body: UploadBody,
        _expected_len: Option<u64>,
        _overwrite: bool,
    ) -> BoxFuture<'_, Result<u64, ExportError>> {
        Box::pin(async { Err(ExportError::Refused("file injection is unavailable".into())) })
    }

    /// Push the session's branch to its repository's default remote.
    fn push_branch(
        &self,
        session_id: String,
        branch: String,
    ) -> BoxFuture<'_, Result<PushedBranch, ExportError>>;

    /// The session's whole tracked work as a patch, from its target while it
    /// has one and from its checkpoint after it stopped.
    fn export_patch(&self, session_id: String) -> BoxFuture<'_, Result<ExportPatch, ExportError>>;

    /// A git bundle of the session's committed work.
    fn bundle(&self, session_id: String) -> BoxFuture<'_, Result<BundleExport, ExportError>>;

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

    fn wiki_search(
        &self,
        _query: String,
        _limit: usize,
    ) -> BoxFuture<'_, AnyResult<mj_client::daemon::WikiSearchPage>> {
        Box::pin(async { anyhow::bail!("SessionWiki search is unavailable") })
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

    /// `None` when the index holds no session with that id.
    fn wiki_brief(
        &self,
        _wiki_id: String,
        _max_chars: usize,
    ) -> BoxFuture<'_, AnyResult<Option<String>>> {
        Box::pin(async { anyhow::bail!("SessionWiki briefings are unavailable") })
    }

    /// The passages of one indexed session that match a query. `None` when the
    /// index holds no session with that id.
    fn wiki_hits(
        &self,
        _wiki_id: String,
        _query: String,
        _context_messages: usize,
        _per_message_chars: usize,
    ) -> BoxFuture<'_, AnyResult<Option<mj_client::daemon::WikiHitTranscript>>> {
        Box::pin(async { anyhow::bail!("SessionWiki transcript hits are unavailable") })
    }

    /// What one indexed session is, and what continuing it would mean.
    /// `None` when the index holds no session with that id.
    fn wiki_session(
        &self,
        _wiki_id: String,
    ) -> BoxFuture<'_, AnyResult<Option<mj_client::daemon::WikiSessionInfo>>> {
        Box::pin(async { anyhow::bail!("SessionWiki lookups are unavailable") })
    }

    fn session_working_context(
        &self,
        _session_id: String,
    ) -> BoxFuture<'_, AnyResult<Option<mj_client::http::SessionWorkingContext>>> {
        Box::pin(async { anyhow::bail!("working context probes are unavailable") })
    }

    fn session_git_status(
        &self,
        _session_id: String,
    ) -> BoxFuture<'_, AnyResult<Option<mj_core::local_git::SessionGitStatus>>> {
        Box::pin(async { anyhow::bail!("git status probes are unavailable") })
    }

    /// Start a session from an archived transcript, answering with its registration, or
    /// `None` when the index holds no session with that id.
    fn wiki_restore(
        &self,
        _request: mj_client::daemon::WikiRestoreRequest,
    ) -> BoxFuture<'_, AnyResult<Option<mj_client::daemon::RegisteredSession>>> {
        Box::pin(async { anyhow::bail!("SessionWiki restore is unavailable") })
    }
}

pub(super) fn backend(state: &ServerState) -> Result<&Arc<dyn SubagentBackend>, ApiFailure> {
    state
        .subagent
        .as_ref()
        .ok_or_else(|| ApiFailure::unavailable("this server has no subagent backend installed"))
}

// ---------------------------------------------------------------------------
// Wait resolution
// ---------------------------------------------------------------------------

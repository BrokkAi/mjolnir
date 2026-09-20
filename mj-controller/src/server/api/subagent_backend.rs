use super::*;

/// Everything the API needs from the daemon: live session actors, the durable
/// projection, and the target-side git operations.
///
/// The daemon's implementation lives in `server_runtime::api`; route tests
/// supply a fake.
pub trait SubagentBackend: Send + Sync {
    fn native_agent_history(
        &self,
        owner: String,
        child: String,
        before: Option<(u64, String)>,
    ) -> BoxFuture<'_, AnyResult<mj_core::native_agent::NativeAgentHistoryPage>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                crate::database::native_agent_history(&owner, &child, before)
            })
            .await?
        })
    }
    fn events(
        &self,
        filter: crate::database::ApiEventFilter,
        after_seq: Option<u64>,
    ) -> BoxFuture<'_, AnyResult<crate::database::ApiEventPage>> {
        events::load_events(filter, after_seq)
    }

    fn profile_config(
        &self,
        profile: String,
        model: Option<String>,
        refresh: bool,
    ) -> BoxFuture<'_, AnyResult<mj_core::worker_launch::ProfileConfig>> {
        Box::pin(crate::controller::profile_config::discover(
            profile, model, refresh,
        ))
    }
    /// What the daemon's background-warmed profile catalogue already holds for
    /// a profile. It never launches a harness and never waits, so a caller a
    /// model is blocked on can check a selector without paying for discovery.
    /// `None` means the catalogue cannot answer yet, not that the profile is
    /// unusable.
    fn published_profile_config(
        &self,
        _profile: &str,
    ) -> Option<mj_core::worker_launch::ProfileConfig> {
        None
    }
    fn start_subagent(
        &self,
        _request: crate::controller::RegisterSubagentRequest,
    ) -> BoxFuture<'_, AnyResult<mj_core::subagent::SubagentRecord>> {
        Box::pin(async { anyhow::bail!("sub-agent creation is unavailable") })
    }
    fn list_subagents(
        &self,
        parent_session_id: String,
    ) -> BoxFuture<'_, AnyResult<Vec<mj_core::subagent::SubagentRecord>>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || crate::database::list_subagents(&parent_session_id))
                .await?
        })
    }
    fn read_context_file(
        &self,
        session_id: String,
        path: PathBuf,
    ) -> BoxFuture<'_, std::result::Result<Vec<u8>, ExportError>> {
        self.read_file(session_id, path)
    }
    /// The workspaces the store holds, in the order the terminal's tabs and the
    /// viewer's list show them.
    fn list_workspaces(
        &self,
    ) -> BoxFuture<'_, AnyResult<Vec<mj_core::workspace::WorkspaceRecord>>> {
        Box::pin(async { tokio::task::spawn_blocking(crate::database::list_workspaces).await? })
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
            tokio::task::spawn_blocking(move || crate::database::create_or_get_workspace(&name))
                .await?
        })
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

    /// Durable turn state for a session with no live actor.
    fn turn_state(&self, session_id: String) -> BoxFuture<'_, AnyResult<Option<TurnState>>>;

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
        role: Option<mj_core::transcript::TranscriptRole>,
    ) -> BoxFuture<'_, AnyResult<Option<TranscriptPage>>>;

    fn usage(
        &self,
        session_id: String,
        after_seq: u64,
        limit: usize,
    ) -> BoxFuture<'_, AnyResult<Option<crate::database::UsagePage>>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                crate::database::load_session_usage(&session_id, after_seq, limit)
            })
            .await?
        })
    }

    /// A unified diff of the session's work.
    fn diff(&self, session_id: String) -> BoxFuture<'_, Result<String, ExportError>>;

    /// One file from the session's workspace.
    fn read_file(
        &self,
        session_id: String,
        path: PathBuf,
    ) -> BoxFuture<'_, Result<Vec<u8>, ExportError>>;

    fn write_file(
        &self,
        _session_id: String,
        _path: PathBuf,
        _bytes: Vec<u8>,
        _overwrite: bool,
    ) -> BoxFuture<'_, Result<(), ExportError>> {
        Box::pin(async { Err(ExportError::Refused("file injection is unavailable".into())) })
    }

    /// Push the session's branch to its repository's default remote.
    fn push_branch(
        &self,
        session_id: String,
        branch: String,
    ) -> BoxFuture<'_, Result<PushedBranch, ExportError>>;

    /// A git bundle of the session's committed work.
    fn bundle(&self, session_id: String) -> BoxFuture<'_, Result<BundleExport, ExportError>>;

    /// Whether the index was synced recently enough that a query need not ask
    /// for one.
    fn wiki_sync_is_stale(&self) -> bool {
        false
    }

    /// Ask for a background sync. It is never waited for: a query answers from
    /// what the index holds now.
    fn wiki_request_sync(&self) {}

    fn wiki_search(
        &self,
        _query: String,
        _limit: usize,
    ) -> BoxFuture<'_, AnyResult<mj_client::daemon::WikiSearchPage>> {
        Box::pin(async { anyhow::bail!("SessionWiki search is unavailable") })
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

    /// Start a session from an archived transcript, answering with its id, or
    /// `None` when the index holds no session with that id.
    fn wiki_restore(
        &self,
        _request: mj_client::daemon::WikiRestoreRequest,
    ) -> BoxFuture<'_, AnyResult<Option<String>>> {
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

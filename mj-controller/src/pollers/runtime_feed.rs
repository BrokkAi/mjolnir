use super::*;

pub struct RemoteDashboardWorkerPoller {
    pub targets: tokio::sync::watch::Sender<Vec<WorkerPollTarget>>,
    pub updates: SessionManagerUpdates,
    pub control: SessionManagerControl,
    pub shutdown: SessionManagerShutdown,
    pub state: tokio::sync::watch::Receiver<RuntimeStateUpdate>,
    /// Reviews the daemon is running for this workspace's sessions.
    pub reviews: tokio::sync::watch::Receiver<Vec<crate::review_host::RuntimeReviewView>>,
    /// Background events the daemon wants reported once, oldest first.
    pub notices: tokio::sync::watch::Receiver<Vec<daemon::RuntimeNotice>>,
    pub config: tokio::sync::watch::Receiver<mj_core::config::Config>,
    pub health: tokio::sync::watch::Receiver<RuntimeFeedHealth>,
}

/// Records and lifecycle ownership must reach the surface in the same frame.
#[derive(Debug, Clone, Default)]
pub struct RuntimeStateUpdate {
    pub native_agents: Vec<mj_core::native_agent::NativeAgentView>,
    pub workspace_names: std::collections::BTreeMap<String, String>,
    pub revision: u64,
    pub records: Vec<SessionRecord>,
    pub lifecycles: Vec<daemon::RuntimeLifecycleView>,
    pub moves: Vec<mj_core::state::MoveOperation>,
    /// Parent/child relations for the sessions in `records`, so a surface can
    /// keep a daemon-created child out of the real workspace without a full
    /// state reload.
    pub subagents: Vec<SubagentRecord>,
}

/// What a session looked like the last time a view was published for it.
///
/// The poller compares this before reading anything, so a session that has not
/// moved costs one comparison rather than a full transcript load. Nothing here
/// grows with the transcript: the projection is identified by its ordinal and
/// digest, and the operational state is bounded by the relay's own command and
/// configuration surface.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PublishedView {
    pub(super) projection_ordinal: u64,
    pub(super) projection_digest: String,
    pub(super) operational: Option<mj_core::relay::RelayOperationalState>,
    pub(super) connected: bool,
    pub(super) error: Option<String>,
}

impl PublishedView {
    pub(super) fn of(runtime: &crate::daemon::RuntimeSessionView) -> Self {
        Self {
            projection_ordinal: runtime.projection_ordinal,
            projection_digest: runtime.projection_digest.clone(),
            operational: runtime.operational.clone(),
            connected: runtime.connected,
            error: runtime.error.as_ref().map(|error| format!("{error:?}")),
        }
    }

    pub(super) fn matches(&self, runtime: &crate::daemon::RuntimeSessionView) -> bool {
        *self == Self::of(runtime)
    }
}

pub(super) const PROJECTION_CONVERGENCE_RETRIES: u8 = 20;
pub(super) const PROJECTION_CONVERGENCE_RETRY_DELAY: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProjectionMismatch {
    pub(super) published_ordinal: u64,
    pub(super) published_digest: String,
    pub(super) durable_ordinal: u64,
    pub(super) durable_digest: String,
}

#[derive(Default)]
pub(super) struct ProjectionConvergence {
    pub(super) attempts: std::collections::BTreeMap<String, (ProjectionMismatch, u8)>,
}

impl ProjectionConvergence {
    pub(super) fn converged(&mut self, session_id: &str) {
        self.attempts.remove(session_id);
    }

    /// Give a lifecycle rollback and the daemon's cached relay view a bounded
    /// window to converge. Repeating the same mismatch eventually reports the
    /// integrity failure instead of hiding it indefinitely.
    pub(super) fn should_retry(&mut self, session_id: &str, mismatch: ProjectionMismatch) -> bool {
        let entry = self
            .attempts
            .entry(session_id.to_owned())
            .or_insert_with(|| (mismatch.clone(), 0));
        if entry.0 != mismatch {
            *entry = (mismatch, 0);
        }
        entry.1 = entry.1.saturating_add(1);
        entry.1 <= PROJECTION_CONVERGENCE_RETRIES
    }
}

/// Read-only updates shared by the dashboard and workspace preview. A snapshot
/// precedes its session views, so consumers can establish membership first.
pub enum RuntimeFeedUpdate {
    Snapshot(Box<daemon::RuntimeSnapshot>),
    Session {
        session_id: String,
        view: Box<ManagedSessionView>,
    },
    Error(String),
}

/// Dropping a subscription cancels even a pending daemon long poll. The task
/// owns no writer or relay connection; blocking projection reads are bounded.
pub struct RuntimeFeed {
    pub updates: tokio::sync::mpsc::Receiver<RuntimeFeedUpdate>,
    pub(super) task: tokio::task::JoinHandle<()>,
}

impl Drop for RuntimeFeed {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) type StoredProjection = Option<(MaterializedSession, mj_core::state::ProjectionWindow)>;

pub(super) static PROJECTION_READERS: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(4)));

pub(super) async fn load_runtime_projection(session_id: String) -> Result<StoredProjection> {
    let permit = Arc::clone(&PROJECTION_READERS)
        .acquire_owned()
        .await
        .context("projection readers stopped")?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let started = Instant::now();
        let result = crate::database::load_materialized_projection_tail(
            &session_id,
            crate::database::PROJECTION_TAIL_ITEMS,
        );
        tracing::debug!(target: "mj_controller::latency", %session_id, elapsed_ms = started.elapsed().as_secs_f64() * 1000.0, "terminal projection loaded");
        // A blocking SQLite read can outlive cancellation of its subscriber.
        if let Err(error) = &result {
            tracing::warn!(%session_id, %error, "could not load runtime projection");
        }
        result
    })
    .await
    .context("projection load task failed")?
}

pub(super) fn spawn_runtime_feed_with<P, PF, L, LF>(
    workspace_id: String,
    poll: P,
    load: L,
) -> RuntimeFeed
where
    P: Fn(String, u64) -> PF + Send + 'static,
    PF: Future<Output = Result<daemon::RuntimeSnapshot>> + Send,
    L: Fn(String) -> LF + Clone + Send + 'static,
    LF: Future<Output = Result<StoredProjection>> + Send + 'static,
{
    let (tx, updates) = tokio::sync::mpsc::channel(32);
    let task = tokio::spawn(async move {
        let result = run_runtime_feed(workspace_id, poll, load, &tx).await;
        if let Err(error) = result {
            let message = format!("Runtime feed stopped: {error:#}");
            tracing::error!(%message);
            let _ = tx.send(RuntimeFeedUpdate::Error(message)).await;
        }
    });
    RuntimeFeed { updates, task }
}

pub(super) async fn run_runtime_feed<P, PF, L, LF>(
    workspace_id: String,
    poll: P,
    load: L,
    tx: &tokio::sync::mpsc::Sender<RuntimeFeedUpdate>,
) -> Result<()>
where
    P: Fn(String, u64) -> PF,
    PF: Future<Output = Result<daemon::RuntimeSnapshot>>,
    L: Fn(String) -> LF + Clone + Send + 'static,
    LF: Future<Output = Result<StoredProjection>> + Send + 'static,
{
    let mut revision = 0;
    let mut convergence = ProjectionConvergence::default();
    let mut published = std::collections::BTreeMap::<String, PublishedView>::new();
    loop {
        let mut snapshot = match poll(workspace_id.clone(), revision).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if tx
                    .send(RuntimeFeedUpdate::Error(format!(
                        "Could not refresh sessions: {error:#}"
                    )))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
        };
        let snapshot_revision = snapshot.revision;
        let sessions = std::mem::take(&mut snapshot.sessions);
        published.retain(|id, _| sessions.iter().any(|session| &session.session_id == id));
        convergence
            .attempts
            .retain(|id, _| sessions.iter().any(|session| &session.session_id == id));
        if tx
            .send(RuntimeFeedUpdate::Snapshot(Box::new(snapshot)))
            .await
            .is_err()
        {
            return Ok(());
        }
        let mut pending = sessions
            .into_iter()
            .filter(|runtime| {
                !published
                    .get(&runtime.session_id)
                    .is_some_and(|last| last.matches(runtime))
            })
            .collect::<std::collections::VecDeque<_>>();
        let mut tasks = tokio::task::JoinSet::new();
        let mut retry = false;
        while !pending.is_empty() || !tasks.is_empty() {
            // Independent session reads overlap, without flooding SQLite or
            // leaving an unbounded number of blocking reads after cancellation.
            while tasks.len() < 4 {
                let Some(runtime) = pending.pop_front() else {
                    break;
                };
                let load = load.clone();
                tasks.spawn(async move {
                    let stored = if runtime.operational.is_some() {
                        load(runtime.session_id.clone()).await
                    } else {
                        Ok(None)
                    };
                    (runtime, stored)
                });
            }
            let Some(result) = tasks.join_next().await else {
                break;
            };
            let (runtime, stored) = result.context("join runtime projection reader")?;
            let session_id = runtime.session_id.clone();
            let fingerprint = PublishedView::of(&runtime);
            let Some(view) = runtime_projection_view(runtime, stored, &mut convergence) else {
                retry = true;
                continue;
            };
            if view.snapshot.is_some() {
                published.insert(session_id.clone(), fingerprint);
            } else {
                published.remove(&session_id);
            }
            if tx
                .send(RuntimeFeedUpdate::Session {
                    session_id,
                    view: Box::new(view),
                })
                .await
                .is_err()
            {
                return Ok(());
            }
        }
        if retry {
            tokio::time::sleep(PROJECTION_CONVERGENCE_RETRY_DELAY).await;
        } else {
            revision = revision.max(snapshot_revision);
        }
    }
}

pub(super) fn runtime_projection_view(
    runtime: daemon::RuntimeSessionView,
    stored: Result<StoredProjection>,
    convergence: &mut ProjectionConvergence,
) -> Option<ManagedSessionView> {
    let Some(operational) = runtime.operational else {
        return Some(ManagedSessionView {
            snapshot: None,
            connected: runtime.connected,
            error: runtime.error,
        });
    };
    let detail = match stored {
        Ok(Some((materialized, window)))
            if materialized.applied_event_ordinal > runtime.projection_ordinal
                || (materialized.applied_event_ordinal == runtime.projection_ordinal
                    && materialized.applied_event_digest == runtime.projection_digest) =>
        {
            convergence.converged(&runtime.session_id);
            return Some(ManagedSessionView {
                snapshot: Some(ManagedSessionSnapshot {
                    materialized,
                    window,
                    operational,
                    latest_credential_sync_signal: runtime.latest_credential_sync_signal,
                    worker_build: None,
                    subagent_requests: Vec::new(),
                    subagent_results: Vec::new(),
                }),
                connected: runtime.connected,
                error: runtime.error,
            });
        }
        Ok(Some((materialized, _))) => {
            let mismatch = ProjectionMismatch {
                published_ordinal: runtime.projection_ordinal,
                published_digest: runtime.projection_digest,
                durable_ordinal: materialized.applied_event_ordinal,
                durable_digest: materialized.applied_event_digest.clone(),
            };
            if convergence.should_retry(&runtime.session_id, mismatch) {
                return None;
            }
            if materialized.applied_event_ordinal < runtime.projection_ordinal {
                format!(
                    "daemon published projection {} but SQLite contains only {} after a bounded convergence retry",
                    runtime.projection_ordinal, materialized.applied_event_ordinal
                )
            } else {
                format!(
                    "daemon and SQLite projection digests differ at ordinal {} after a bounded convergence retry",
                    runtime.projection_ordinal
                )
            }
        }
        Ok(None) => "daemon published a session with no durable projection".into(),
        Err(error) => format!("load daemon-owned projection: {error:#}"),
    };
    Some(ManagedSessionView {
        snapshot: None,
        connected: false,
        error: Some(ViewError::ProjectionIntegrity(detail)),
    })
}

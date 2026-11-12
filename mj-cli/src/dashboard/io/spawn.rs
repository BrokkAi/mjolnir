use super::*;

/// Hands one finished answer to the dashboard. A closed channel means the
/// dashboard has already shut down, which is not a failure, so every helper
/// reports through here instead of deciding that for itself.
pub(crate) fn report<T>(operation: &str, updates: &UnboundedSender<T>, update: T) {
    if let Err(error) = updates.send(update) {
        tracing::debug!(operation, %error, "dashboard background result dropped after shutdown");
    }
}

/// Turns a finished blocking job into the answer the dashboard shows. A panic
/// in the job arrives here as a join error and becomes a reported failure, so
/// the screen never waits forever on work that died.
fn blocking_result<T>(
    operation: &str,
    joined: std::result::Result<Result<T>, JoinError>,
) -> std::result::Result<T, String> {
    match joined {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => {
            let error = format!("{error:#}");
            tracing::warn!(operation, %error, "dashboard background operation failed");
            Err(error)
        }
        Err(error) => {
            let error = format!("{operation} task failed: {error}");
            tracing::warn!(operation, %error, "dashboard background operation panicked");
            Err(error)
        }
    }
}

/// Runs one blocking job off the loop and reports its outcome on the
/// dashboard's I/O channel. Errors and panics are formatted once, here, so no
/// caller can quietly drop one.
pub(crate) fn spawn_io<T>(
    operation: &'static str,
    updates: UnboundedSender<DashboardIoUpdate>,
    work: impl FnOnce() -> Result<T> + Send + 'static,
    to_update: impl FnOnce(std::result::Result<T, String>) -> DashboardIoUpdate + Send + 'static,
) -> JoinHandle<()>
where
    T: Send + 'static,
{
    tokio::spawn(async move {
        let result = blocking_result(operation, tokio::task::spawn_blocking(work).await);
        report(operation, &updates, to_update(result));
    })
}

/// Runs a user-authored mutation off the event loop and keeps dashboard exit
/// pending until the mutation has reached its durable boundary.
pub(crate) fn spawn_critical_io<T>(
    tracker: CriticalOperationTracker,
    label: impl Into<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    work: impl FnOnce() -> Result<T> + Send + 'static,
    to_update: impl FnOnce(std::result::Result<T, String>) -> DashboardIoUpdate + Send + 'static,
) -> JoinHandle<()>
where
    T: Send + 'static,
{
    let label = label.into();
    let guard = tracker.begin(label.clone());
    tokio::spawn(async move {
        let result = blocking_result(&label, tokio::task::spawn_blocking(work).await);
        report(&label, &updates, to_update(result));
        drop(guard);
    })
}

/// Network waits must not hold a blocking-pool thread: connecting to the
/// daemon itself needs a blocking metadata read. Bound acknowledgement waits
/// so a silent daemon cannot indefinitely prevent dashboard exit.
pub(crate) fn spawn_async_job<T: Send + 'static>(
    tracker: Option<CriticalOperationTracker>,
    label: impl Into<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    timeout: Duration,
    work: impl std::future::Future<Output = Result<T>> + Send + 'static,
    to_update: impl FnOnce(std::result::Result<T, String>) -> DashboardIoUpdate + Send + 'static,
) -> JoinHandle<()> {
    let label = label.into();
    let guard = tracker.map(|tracker| tracker.begin(label.clone()));
    tokio::spawn(async move {
        let task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(work));
        let result = match tokio::time::timeout(timeout, task).await {
            Ok(Ok(result)) => result.map_err(|error| format!("{error:#}")),
            Ok(Err(error)) => Err(format!("{label} task failed: {error}")),
            Err(_) => Err(format!(
                "{label}: the daemon did not acknowledge the save within {} seconds; it may still complete. Reconnect to verify the saved state",
                timeout.as_secs()
            )),
        };
        if let Err(error) = &result {
            tracing::error!(operation = %label, %error, "dashboard save was not confirmed");
        }
        report(&label, &updates, to_update(result));
        drop(guard);
    })
}

pub(crate) fn spawn_critical_async<T: Send + 'static>(
    tracker: CriticalOperationTracker,
    label: impl Into<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    timeout: Duration,
    work: impl std::future::Future<Output = Result<T>> + Send + 'static,
    to_update: impl FnOnce(std::result::Result<T, String>) -> DashboardIoUpdate + Send + 'static,
) -> JoinHandle<()> {
    spawn_async_job(Some(tracker), label, updates, timeout, work, to_update)
}

pub(crate) fn spawn_background_async<T: Send + 'static>(
    label: impl Into<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    timeout: Duration,
    work: impl std::future::Future<Output = Result<T>> + Send + 'static,
    to_update: impl FnOnce(std::result::Result<T, String>) -> DashboardIoUpdate + Send + 'static,
) -> JoinHandle<()> {
    spawn_async_job(None, label, updates, timeout, work, to_update)
}

/// Loads the manager's complete view from the daemon. Workspace listings and
/// their detached-draft previews are read through the same client so the
/// snapshot is ordered and every failure reaches the dashboard.
pub(crate) async fn load_workspace_management_entries(
    daemon: &mut daemon::DaemonClient,
) -> Result<Vec<WorkspaceManagementEntry>> {
    let listings = daemon.list_workspaces().await?;
    let mut entries = Vec::with_capacity(listings.len());
    for listing in listings {
        let snapshot = daemon.snapshot(listing.workspace.id.clone()).await?;
        let drafts = snapshot
            .drafts
            .into_iter()
            .map(|draft| WorkspaceDraftEntry {
                id: draft.id,
                session_id: draft.session_id,
                source: draft.source,
                saved_at: draft.saved_at,
                owner_pid: draft.owner_pid,
            })
            .collect();
        entries.push(WorkspaceManagementEntry {
            workspace: snapshot.workspace,
            drafts,
        });
    }
    Ok(entries)
}

pub(crate) fn spawn_workspace_management_operation(
    generation: u64,
    label: impl Into<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: Option<CriticalOperationTracker>,
    work: impl std::future::Future<Output = Result<WorkspaceManagementResult>> + Send + 'static,
) -> JoinHandle<()> {
    let report = move |result| DashboardIoUpdate::WorkspaceManagement { generation, result };
    match tracker {
        Some(tracker) => {
            spawn_critical_async(tracker, label, updates, SAVE_ACK_TIMEOUT, work, report)
        }
        None => spawn_background_async(label, updates, SAVE_ACK_TIMEOUT, work, report),
    }
}

pub(crate) fn spawn_workspace_management_load(
    generation: u64,
    updates: UnboundedSender<DashboardIoUpdate>,
) -> JoinHandle<()> {
    spawn_workspace_management_operation(generation, "loading workspaces", updates, None, async {
        let mut daemon = daemon::connect_or_start().await?;
        Ok(WorkspaceManagementResult {
            entries: load_workspace_management_entries(&mut daemon).await?,
            select_workspace: None,
            deleted_workspace_id: None,
        })
    })
}

/// Reads layouts for workspace ids discovered after the dashboard opened.
/// Loading is deliberately separate from the runtime snapshot so a late
/// result cannot overwrite a pane size the local client already edited.
pub(crate) fn spawn_workspace_pane_sizes_load(
    workspace_ids: Vec<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
) -> JoinHandle<()> {
    spawn_background_async(
        "load workspace pane sizes",
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            Ok(daemon::connect_or_start()
                .await?
                .workspace_layouts(workspace_ids)
                .await?
                .pane_sizes
                .into_iter()
                .collect())
        },
        |result| DashboardIoUpdate::WorkspacePaneSizes { result },
    )
}

/// Reads conversation layouts for workspace ids discovered after the dashboard
/// opened, for the same reason pane sizes are read separately.
pub(crate) fn spawn_workspace_layouts_load(
    workspace_ids: Vec<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
) -> JoinHandle<()> {
    spawn_background_async(
        "load workspace layouts",
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            Ok(daemon::connect_or_start()
                .await?
                .workspace_layouts(workspace_ids)
                .await?
                .conversation_layouts
                .into_iter()
                .collect())
        },
        |result| DashboardIoUpdate::WorkspaceLayouts { result },
    )
}

pub(crate) fn spawn_workspace_create(
    generation: u64,
    name: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_workspace_management_operation(
        generation,
        "creating workspace",
        updates,
        Some(tracker),
        async move {
            let mut daemon = daemon::connect_or_start().await?;
            let workspace = daemon.create_workspace(name).await?;
            Ok(WorkspaceManagementResult {
                entries: load_workspace_management_entries(&mut daemon).await?,
                select_workspace: Some(workspace.id),
                deleted_workspace_id: None,
            })
        },
    )
}

pub(crate) fn spawn_workspace_rename(
    generation: u64,
    workspace_id: String,
    name: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_workspace_management_operation(
        generation,
        "renaming workspace",
        updates,
        Some(tracker),
        async move {
            let mut daemon = daemon::connect_or_start().await?;
            daemon.rename_workspace(workspace_id, name).await?;
            Ok(WorkspaceManagementResult {
                entries: load_workspace_management_entries(&mut daemon).await?,
                select_workspace: None,
                deleted_workspace_id: None,
            })
        },
    )
}

pub(crate) fn spawn_workspace_close_cancel(
    workspace_id: String,
    updates: UnboundedSender<DashboardIoUpdate>,
) -> JoinHandle<()> {
    spawn_background_async(
        "cancel workspace close",
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            daemon::connect_or_start()
                .await?
                .cancel_workspace_close(workspace_id)
                .await
        },
        |result| DashboardIoUpdate::WorkspaceCloseCancelled { result },
    )
}

pub(crate) fn spawn_workspace_close(
    generation: u64,
    workspace_id: String,
    updates: UnboundedSender<DashboardIoUpdate>,
) -> JoinHandle<()> {
    // The daemon owns stopping. Detaching the UI must not wait for checkpoints,
    // and ordinary save acknowledgement deadlines are too short for them.
    tokio::spawn(async move {
        let closing_workspace_id = workspace_id.clone();
        let work = tokio::spawn(async move {
            let mut daemon = daemon::connect_or_start().await?;
            daemon.close_workspace(workspace_id).await
        });
        let result = blocking_result("closing workspace", work.await);
        report(
            "closing workspace",
            &updates,
            DashboardIoUpdate::WorkspaceClosed {
                generation,
                workspace_id: closing_workspace_id,
                result,
            },
        );
    })
}

pub(crate) fn spawn_workspace_draft_recovery(
    generation: u64,
    draft_id: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_workspace_management_operation(
        generation,
        "recovering workspace draft",
        updates,
        Some(tracker),
        async move {
            let mut daemon = daemon::connect_or_start().await?;
            daemon.recover_draft(draft_id).await?;
            Ok(WorkspaceManagementResult {
                entries: load_workspace_management_entries(&mut daemon).await?,
                select_workspace: None,
                deleted_workspace_id: None,
            })
        },
    )
}

pub(crate) const SAVE_ACK_TIMEOUT: Duration = Duration::from_secs(15);

/// Like [`spawn_critical_io`], with a cooperative cancellation flag for work
/// that can own a subprocess while the dashboard is shutting down.
pub(crate) fn spawn_cancellable_io<T>(
    tracker: CriticalOperationTracker,
    label: impl Into<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    work: impl FnOnce(Arc<AtomicBool>) -> Result<T> + Send + 'static,
    to_update: impl FnOnce(std::result::Result<T, String>) -> DashboardIoUpdate + Send + 'static,
) -> JoinHandle<()>
where
    T: Send + 'static,
{
    spawn_cancellable_io_with_token(tracker, label, updates, work, to_update).1
}

pub(crate) fn spawn_cancellable_io_with_token<T>(
    tracker: CriticalOperationTracker,
    label: impl Into<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    work: impl FnOnce(Arc<AtomicBool>) -> Result<T> + Send + 'static,
    to_update: impl FnOnce(std::result::Result<T, String>) -> DashboardIoUpdate + Send + 'static,
) -> (Arc<AtomicBool>, JoinHandle<()>)
where
    T: Send + 'static,
{
    let label = label.into();
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = tracker.begin_cancellable(label.clone(), cancelled.clone());
    let worker_cancelled = cancelled.clone();
    let worker = tokio::spawn(async move {
        let joined = tokio::task::spawn_blocking(move || work(worker_cancelled)).await;
        let result = blocking_result(&label, joined);
        report(&label, &updates, to_update(result));
        drop(guard);
    });
    (cancelled, worker)
}

/// Discover and save first-run configuration without blocking terminal input.
pub(crate) fn spawn_first_run_setup(
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    let progress = updates.clone();
    spawn_cancellable_io(
        tracker,
        "first-run setup",
        updates,
        move |_cancelled| {
            report(
                "first-run setup",
                &progress,
                DashboardIoUpdate::FirstRunStarted,
            );
            // The daemon owns its configuration and its environment. In
            // container placement this discovery runs inside the container,
            // not against the client's host home.
            let (summary, errors) = mj_core::runtime::block_on(async {
                let mut daemon = crate::daemon::connect_or_start().await?;
                let summary = daemon.run_first_run_setup().await?;
                let errors = if summary.is_some() {
                    daemon.run_doctor().await?
                } else {
                    Vec::new()
                };
                Ok::<_, anyhow::Error>((summary, errors))
            })??;
            // An existing installation picks up agents installed since its
            // last launch through the startup availability check.
            let Some(summary) = summary else {
                return Ok(None);
            };
            report(
                "first-run setup",
                &progress,
                DashboardIoUpdate::FirstRunConfigured(summary),
            );
            Ok(Some(errors))
        },
        DashboardIoUpdate::FirstRunChecked,
    )
}

/// Check what is there and add what is new, off the event loop: this
/// computer's agents and engines, and every machine at once, each with one
/// SSH call. Quitting cancels the check.
pub(crate) fn spawn_availability_check(
    generation: u64,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_cancellable_io(
        tracker,
        "checking what is available",
        updates,
        move |cancelled| match machine_request(
            mj_client::daemon::DaemonAction::CheckAvailability,
            cancelled,
        )? {
            mj_client::daemon::DaemonReply::Availability(report) => Ok(report),
            _ => anyhow::bail!("daemon returned an unexpected availability reply"),
        },
        move |result| DashboardIoUpdate::AvailabilityChecked { generation, result },
    )
}

/// Read saved-profile sign-in status off the dashboard's loop.
pub(crate) fn spawn_profile_login_statuses(
    generation: u64,
    updates: UnboundedSender<DashboardIoUpdate>,
) -> JoinHandle<()> {
    spawn_io(
        "reading profile sign-in status",
        updates,
        move || match machine_request(
            mj_client::daemon::DaemonAction::ProfileLoginStatuses,
            Arc::new(AtomicBool::new(false)),
        )? {
            mj_client::daemon::DaemonReply::ProfileLoginStatuses(statuses) => Ok(statuses),
            _ => anyhow::bail!("daemon returned an unexpected profile login status reply"),
        },
        move |result| DashboardIoUpdate::ProfileLoginStatuses { generation, result },
    )
}

pub(crate) fn spawn_setup_discovery(
    generation: u64,
    scope: DetectScope,
    machines: Vec<(String, mj_core::config::Machine)>,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_cancellable_io(
        tracker,
        match scope {
            DetectScope::Profiles => "detecting agent profiles",
            DetectScope::Runtimes => "detecting runtimes",
        },
        updates,
        move |cancelled| match machine_request(
            mj_client::daemon::DaemonAction::DiscoverSetup { scope, machines },
            cancelled,
        )? {
            mj_client::daemon::DaemonReply::SetupDiscovery(report) => Ok(*report),
            _ => anyhow::bail!("daemon returned an unexpected discovery reply"),
        },
        move |result| DashboardIoUpdate::SetupDiscovered { generation, result },
    )
}

#[cfg(test)]
pub(crate) use mj_client::settings::save::save_at as save_setup_at;

pub(crate) fn spawn_setup_save(
    generation: u64,
    original: String,
    updated: String,
    login_profile: Option<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_critical_async(
        tracker,
        "saving setup",
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            // The daemon checks the edit against the stored sessions it would
            // disturb; only it opens the store.
            daemon::connect_or_start()
                .await?
                .save_setup(original, updated)
                .await
        },
        move |result| DashboardIoUpdate::SetupSaved {
            generation,
            result,
            login_profile,
        },
    )
}

pub(crate) fn spawn_spinner_style_save(
    style: mj_core::config::SpinnerStyle,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_critical_io(
        tracker,
        "saving spinner style",
        updates,
        move || {
            Config::update(|config| {
                config.spinner = style;
                Ok(())
            })
            .map(|(config, ())| config)
        },
        |result| DashboardIoUpdate::SpinnerStyleSaved { result },
    )
}

/// Resolves one raw checkout's Git origin off the event loop. Each session is
/// independent, so callers can launch these concurrently and redraw as the
/// answers arrive.
pub(crate) fn spawn_project_source_resolution(
    session_id: String,
    session_name: &str,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    let reported_session_id = session_id.clone();
    spawn_cancellable_io(
        tracker,
        format!("resolving project for {session_name}"),
        updates,
        move |_cancelled| {
            mj_core::runtime::block_on(async {
                daemon::connect_or_start()
                    .await?
                    .session_project_source(session_id)
                    .await
            })?
        },
        move |result| DashboardIoUpdate::ProjectSource {
            session_id: reported_session_id,
            result,
        },
    )
}

/// A controller that answers target questions from configuration alone.
pub(crate) use mj_controller::controller::config_only_controller;

/// What every session lifecycle operation needs to run off the loop.
pub(crate) struct LifecycleOperationRequest {
    pub(crate) session_id: String,
    /// The session's name as notices give it, for the operation's label.
    pub(crate) session_name: String,
    pub(crate) kind: SessionOperationKind,
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) updates: UnboundedSender<DashboardLifecycleUpdate>,
}

/// Keeps client ownership until the daemon confirms lifecycle admission.
pub(crate) struct LifecycleSubmission(
    Arc<std::sync::Mutex<Option<mj_client::operations::CriticalOperationGuard>>>,
);

impl LifecycleSubmission {
    pub(crate) async fn submit(
        self,
        client: &mut daemon::DaemonClient,
        request: daemon::SessionLifecycleRequest,
    ) -> Result<daemon::LifecycleReceipt> {
        let receipt = client.start_session_lifecycle(request).await?;
        self.release();
        Ok(receipt)
    }

    fn release(&self) {
        let guard = self
            .0
            .lock()
            .expect("lifecycle submission ownership")
            .take();
        drop(guard);
    }
}

/// Protect submission, then follow daemon-owned work without holding quit.
pub(crate) fn spawn_lifecycle_operation(
    request: LifecycleOperationRequest,
    tracker: CriticalOperationTracker,
    work: impl FnOnce(Arc<AtomicBool>, LifecycleSubmission) -> Result<LifecycleSuccess> + Send + 'static,
) {
    let LifecycleOperationRequest {
        session_id,
        session_name,
        kind,
        cancelled,
        updates,
    } = request;
    let submission = LifecycleSubmission(Arc::new(std::sync::Mutex::new(Some(tracker.begin(
        format!(
            "submitting {} for session {session_name}",
            kind.label().to_ascii_lowercase()
        ),
    )))));
    tokio::spawn(async move {
        let worker_cancelled = cancelled.clone();
        // Failed or panicking submission must report its uncertainty before
        // quit is released; successful submission releases at the receipt.
        let worker_submission = LifecycleSubmission(Arc::clone(&submission.0));
        let result =
            match tokio::task::spawn_blocking(move || work(worker_cancelled, worker_submission))
                .await
            {
                Ok(result) => result.map_err(crate::pollers::DashboardLifecycleFailure::from),
                Err(error) => Err(crate::pollers::DashboardLifecycleFailure::Failed(format!(
                    "session lifecycle operation task failed: {error}"
                ))),
            };
        report(
            "session lifecycle operation",
            &updates,
            DashboardLifecycleUpdate {
                session_id,
                result,
                operation: cancelled.clone(),
            },
        );
        submission.release();
    });
}

pub(crate) fn spawn_materialized_session_projection(
    projection: crate::dashboard::MaterializedProjection,
    viewed_through_event_ordinal: u64,
    previous: mj_tui::MaterializedProjectionCache,
    updates: UnboundedSender<DashboardIoUpdate>,
    permits: Arc<tokio::sync::Semaphore>,
) {
    let session_id = projection.materialized.session_id.clone();
    tokio::spawn(async move {
        let result = match permits.acquire_owned().await {
            Ok(permit) => {
                let result = tokio::task::spawn_blocking(move || {
                    PreparedMaterializedSessionDetail::from_assessed_materialized(
                        projection.materialized,
                        projection.assessment.as_deref(),
                        viewed_through_event_ordinal,
                        previous,
                    )
                })
                .await
                .map(Box::new)
                .map_err(|error| format!("session projection task failed: {error}"));
                drop(permit);
                result
            }
            Err(error) => Err(format!("session projection worker stopped: {error}")),
        };
        report(
            "session projection",
            &updates,
            DashboardIoUpdate::MaterializedSessionProjection { session_id, result },
        );
    });
}

/// Load the stored tail of a stopped sub-agent's conversation: as much as a
/// live chat keeps, off the event loop.
pub(crate) fn spawn_stopped_subagent_transcript(
    session_id: String,
    updates: UnboundedSender<DashboardIoUpdate>,
) {
    let reported_session_id = session_id.clone();
    spawn_background_async(
        "load a stopped sub-agent's transcript",
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            daemon::connect_or_start()
                .await?
                .stored_projection_tail(session_id, mj_chat::chat::TAIL_SEED_ITEMS)
                .await
        },
        move |result| DashboardIoUpdate::StoppedSubagentTranscript {
            session_id: reported_session_id,
            result,
        },
    );
}

pub(crate) fn spawn_stored_session_summary(
    session_id: String,
    viewed_through_event_ordinal: u64,
    generation: u64,
    permits: Arc<tokio::sync::Semaphore>,
    updates: UnboundedSender<DashboardIoUpdate>,
) -> JoinHandle<()> {
    let reported_session_id = session_id.clone();
    tokio::spawn(async move {
        let _permit = permits
            .acquire_owned()
            .await
            .expect("summary read permits remain open");
        let task = spawn_background_async(
            "load stored session summary",
            updates,
            SAVE_ACK_TIMEOUT,
            async move {
                let summary = daemon::connect_or_start()
                    .await?
                    .stored_session_summary(session_id.clone())
                    .await?
                    .with_context(|| format!("session {session_id} has no stored projection"))?;
                // Only CPU preparation needs a blocking thread. Acquiring one
                // while awaiting the connection can starve its metadata read.
                tokio::task::spawn_blocking(move || {
                    PreparedMaterializedSessionSummary::from_materialized(
                        summary,
                        viewed_through_event_ordinal,
                    )
                })
                .await
                .context("prepare stored session summary task failed")
            },
            move |result| DashboardIoUpdate::StoredSessionSummary {
                session_id: reported_session_id,
                generation,
                result,
            },
        );
        if let Err(error) = tokio_util::task::AbortOnDropHandle::new(task).await {
            tracing::error!(%error, "stored summary supervisor failed");
        }
    })
}

pub(crate) fn spawn_lifecycle_reload(
    reload: LifecycleReload,
    updates: UnboundedSender<DashboardIoUpdate>,
) {
    spawn_background_async(
        "reload lifecycle state",
        updates,
        SAVE_ACK_TIMEOUT,
        crate::dashboard::io::stored_session_metadata(),
        move |result| {
            DashboardIoUpdate::LifecycleReloaded(Box::new(LifecycleReloaded { reload, result }))
        },
    );
}

pub(crate) fn spawn_dashboard_rename(
    session_id: String,
    session_name: &str,
    title: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    let renamed_session_id = session_id.clone();
    let requested_title = title.clone();
    spawn_critical_async(
        tracker,
        format!("renaming session {session_name}"),
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            daemon::connect_or_start()
                .await?
                .set_session_title(renamed_session_id, requested_title)
                .await
        },
        move |result| DashboardIoUpdate::RenameSession { title, result },
    );
}

pub(crate) fn spawn_dashboard_change_workspace(
    session_id: String,
    session_name: &str,
    workspace_id: String,
    workspace_name: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    let moved_session_id = session_id.clone();
    spawn_critical_async(
        tracker,
        format!("moving session {session_name} to a workspace"),
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            daemon::connect_or_start()
                .await?
                .set_session_workspace(moved_session_id, workspace_id)
                .await
        },
        move |result| DashboardIoUpdate::ChangeWorkspace {
            session_id,
            workspace_name,
            result,
        },
    );
}

pub(crate) struct ConfigRenameRequest {
    pub(crate) what: String,
    pub(crate) old_id: String,
    pub(crate) new_id: String,
    pub(crate) profile: bool,
}

pub(crate) fn spawn_config_rename(
    request: ConfigRenameRequest,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    let ConfigRenameRequest {
        what,
        old_id,
        new_id,
        profile,
    } = request;
    spawn_critical_async(
        tracker,
        format!("renaming {what}"),
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            let mut daemon = daemon::connect_existing().await?;
            if profile {
                daemon.rename_profile(old_id, new_id).await?;
            } else {
                daemon.rename_target(old_id, new_id).await?;
            }
            daemon
                .stored_session_metadata()
                .await
                .map(DashboardMetadata::from)
        },
        move |result| DashboardIoUpdate::ConfigRename { what, result },
    );
}

/// Asks the daemon, the only process that probes quota, to probe now. The
/// request returns once the daemon has accepted it; the reports arrive through
/// the runtime feed.
pub(crate) fn spawn_quota_refresh_request(updates: UnboundedSender<DashboardIoUpdate>) {
    spawn_background_async(
        "asking the daemon to refresh quota",
        updates,
        SAVE_ACK_TIMEOUT,
        async move { daemon::connect_existing().await?.refresh_quota().await },
        DashboardIoUpdate::QuotaRefreshRequested,
    );
}

/// What the container editor asks the controller to persist.
pub(crate) struct ContainerSettingsRequest {
    pub(crate) session_id: String,
    pub(crate) cpus: Option<String>,
    pub(crate) memory: Option<String>,
    pub(crate) additional_mounts: Vec<mj_controller::targets::AdditionalMount>,
    pub(crate) mount_history: Vec<std::path::PathBuf>,
}

pub(crate) fn spawn_dashboard_container_settings(
    request: ContainerSettingsRequest,
    session_name: &str,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    let session_id = request.session_id.clone();
    spawn_critical_async(
        tracker,
        format!("saving container settings for {session_name}"),
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            let ContainerSettingsRequest {
                session_id,
                cpus,
                memory,
                additional_mounts,
                mount_history,
            } = request;
            daemon::connect_or_start()
                .await?
                .set_session_container_settings(
                    session_id,
                    cpus,
                    memory,
                    additional_mounts,
                    mount_history,
                )
                .await?;
            // Return a fresh durable snapshot so the dashboard can update its
            // state without synchronously reloading the database while it is
            // applying the worker result.
            crate::dashboard::io::stored_session_metadata().await
        },
        move |result| DashboardIoUpdate::ContainerSettings { session_id, result },
    );
}

/// A session by its id and by the name notices give it, for background jobs
/// whose label names the session they work on.
pub(crate) struct NamedSession {
    pub(crate) id: String,
    pub(crate) name: String,
}

/// One revisioned owner write; the cache sequences and coalesces each session.
pub(crate) fn spawn_terminal_draft_persist(
    client_id: String,
    session: NamedSession,
    revision: u64,
    draft: DetachedSessionDraft,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    let NamedSession {
        id: session_id,
        name: session_name,
    } = session;
    let persisted_session_id = session_id.clone();
    spawn_critical_async(
        tracker,
        format!("saving draft for {session_name}"),
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            daemon::connect_or_start()
                .await?
                .persist_terminal_draft(
                    client_id,
                    persisted_session_id,
                    std::process::id(),
                    revision,
                    draft,
                )
                .await
        },
        move |result| DashboardIoUpdate::TerminalDraft {
            session_id,
            revision,
            result,
        },
    )
}

pub(crate) fn spawn_read_receipt_persist(
    client_id: String,
    workspace_id: String,
    session: NamedSession,
    through: u64,
    retry_delay: Duration,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    let NamedSession {
        id: session_id,
        name: session_name,
    } = session;
    let persisted_session_id = session_id.clone();
    spawn_critical_async(
        tracker,
        format!("saving read status for {session_name}"),
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            // Give projection persistence or a reconnect time to catch up.
            tokio::time::sleep(retry_delay).await;
            daemon::connect_or_start()
                .await?
                .persist_read_receipt(client_id, workspace_id, persisted_session_id, through)
                .await
        },
        move |result| DashboardIoUpdate::ReadReceipt { session_id, result },
    );
}

pub(crate) fn spawn_clipboard_read(
    target: mj_tui::DashboardPasteTarget,
    updates: UnboundedSender<DashboardIoUpdate>,
) -> JoinHandle<()> {
    spawn_io(
        "read clipboard",
        updates,
        mj_chat::clipboard::read_text,
        move |result| DashboardIoUpdate::ClipboardText { target, result },
    )
}

/// Writes copied text to the desktop clipboard off the render loop.
pub(crate) fn spawn_clipboard_write(
    text: String,
    updates: UnboundedSender<DashboardIoUpdate>,
) -> JoinHandle<()> {
    spawn_io(
        "write clipboard",
        updates,
        move || mj_chat::clipboard::write_text(&text),
        DashboardIoUpdate::ClipboardWritten,
    )
}

pub(crate) fn spawn_create_bundle(
    sources: Vec<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    spawn_async_job(
        Some(tracker),
        "creating project",
        updates,
        Duration::from_secs(60),
        async move {
            let mut daemon = daemon::connect_or_start().await?;
            let created = daemon.create_project(sources).await?;
            Ok(CreatedBundleUpdate {
                bundle: created.project.bundle,
                bundle_id: created.bundle_id,
            })
        },
        |result| DashboardIoUpdate::CreatedBundle {
            result: Box::new(result),
        },
    );
}

pub(crate) fn spawn_remove_bundle(
    bundle_id: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    let removed = bundle_id.clone();
    spawn_critical_async(
        tracker,
        "removing project",
        updates,
        Duration::from_secs(60),
        async move {
            daemon::connect_or_start()
                .await?
                .remove_project(bundle_id)
                .await
        },
        move |result| DashboardIoUpdate::RemovedBundle {
            bundle_id: removed,
            result,
        },
    );
}

pub(crate) fn spawn_imported_session_apply(
    mut imported: DashboardImportSuccess,
    pending: PendingDashboardImport,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    spawn_critical_io(
        tracker,
        "saving imported session",
        updates,
        move || {
            let session = imported
                .controller
                .state
                .sessions
                .remove(&imported.session_id)
                .context("import worker did not return its new session")?;
            let bundle = imported
                .controller
                .config
                .bundles
                .get(&session.bundle_id)
                .cloned()
                .context("import worker did not return its session bundle")?;
            persist_imported_session(
                &session,
                Some(crate::daemon::ImportedBundle {
                    bundle_id: session.bundle_id.clone(),
                    bundle: bundle.clone(),
                }),
            )?;
            Ok(ImportedDashboardSessionApply {
                harness: imported.harness,
                native_session_id: pending.native_session_id,
                session,
                bundle,
            })
        },
        |result| DashboardIoUpdate::ImportedSessionApplied {
            result: Box::new(result),
        },
    );
}

/// Loads the stopped sessions the resume dialog lists. The runtime feed
/// carries only live sessions.
pub(crate) fn spawn_resume_candidates(
    discovery_id: u64,
    updates: UnboundedSender<DashboardIoUpdate>,
) -> JoinHandle<()> {
    spawn_background_async(
        "load suspended sessions",
        updates,
        SAVE_ACK_TIMEOUT,
        async { daemon::connect_or_start().await?.resume_candidates().await },
        move |result| DashboardIoUpdate::ResumeCandidates {
            discovery_id,
            result: Box::new(result),
        },
    )
}

/// Fetches the whole record of the session picked in the resume dialog.
pub(crate) fn spawn_resume_record(
    session_id: String,
    updates: UnboundedSender<DashboardIoUpdate>,
) -> JoinHandle<()> {
    let requested = session_id.clone();
    spawn_background_async(
        "load session to resume",
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            daemon::connect_or_start()
                .await?
                .session_record(requested)
                .await
        },
        move |result| DashboardIoUpdate::ResumeRecord {
            session_id,
            result: Box::new(result),
        },
    )
}

pub(crate) fn spawn_checkpoint_archive_size_refresh(
    generation: u64,
    targets: BTreeMap<String, PathBuf>,
    updates: UnboundedSender<DashboardIoUpdate>,
) {
    tokio::task::spawn_blocking(move || {
        let sizes = targets
            .iter()
            .map(|(session_id, path)| {
                let size = match std::fs::metadata(path) {
                    Ok(metadata) => Some(metadata.len()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => {
                        tracing::warn!(path = %path.display(), %error, "could not read checkpoint archive size");
                        None
                    }
                };
                (session_id.clone(), size)
            })
            .collect();
        report(
            "checkpoint archive sizes",
            &updates,
            DashboardIoUpdate::CheckpointArchiveSizes {
                generation,
                targets,
                sizes,
            },
        );
    });
}

/// Registering a session and provisioning it are one job with two answers: the
/// dashboard shows the session as soon as it exists, then follows the launch,
/// so this stays separate from [`spawn_lifecycle_operation`]. Only the
/// registration holds dashboard exit: once the daemon has the record, the
/// launch is the daemon's.
pub(crate) fn spawn_dashboard_create_session(
    action: DashboardAction,
    go_save: Option<(std::path::PathBuf, mj_core::go::GoRecipe, bool)>,
    updates: UnboundedSender<DashboardIoUpdate>,
    lifecycle_updates: UnboundedSender<DashboardLifecycleUpdate>,
    tracker: CriticalOperationTracker,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = tracker.begin_cancellable("creating session", cancelled.clone());
    tokio::task::spawn_blocking(move || {
        let retry_launch = action.clone();
        let DashboardAction::CreateSession {
            remember_container_size,
            create_managed_worktree,
            subagents,
            oracle,
            oracle_choice,
            review,
            workspace_id,
            profile_id,
            bundle_id,
            project_directory,
            target_template_id,
            additional_mounts,
            resource_allocation,
        } = action.clone()
        else {
            return;
        };
        let registered = (|| -> Result<Option<RegisteredDashboardSession>> {
            if let Some((directory, recipe, global)) = go_save {
                mj_core::go::GoPreferences::save_recipe(
                    &mj_core::go::GoPreferences::path(),
                    directory,
                    recipe,
                    global,
                )
                .context("save fast-start setup; session has not been started")?;
            }
            // The new-session preflight reads configuration and runs git in
            // the environment that owns the project paths. A client in another
            // placement cannot do that itself.
            if project_directory.is_none() {
                let repairs = mj_core::runtime::block_on(async {
                    daemon::connect_or_start()
                        .await?
                        .new_session_remote_repairs(bundle_id.clone(), target_template_id.clone())
                        .await
                })??;
                if !repairs.is_empty() {
                    updates
                        .send(DashboardIoUpdate::CreateSession(Box::new(
                            DashboardCreateSessionUpdate::RemoteRepair {
                                bundle_id: bundle_id.clone(),
                                repairs,
                                retry: Box::new(action.clone()),
                            },
                        )))
                        .context("dashboard closed during remote repair preparation")?;
                    return Ok(None);
                }
            }
            if cancelled.load(Ordering::Acquire) {
                bail!("operation cancelled");
            }
            let title = mj_core::state::default_session_title(
                project_directory.as_deref(),
                &bundle_id,
                &profile_id,
            );
            let registered = mj_core::runtime::block_on(async {
                daemon::connect_or_start()
                    .await?
                    .start_create_session(daemon::CreateSessionRequest {
                        oracle,
                        oracle_choice,
                        remember_launch_choices: true,
                        remember_container_size,
                        review,
                        no_project_memory: false,
                        no_github_events: false,
                        no_github_credentials: false,
                        at: None,
                        branch: None,
                        base: None,
                        no_later_history: false,
                        subagents,
                        create_managed_worktree,
                        initial_prompt: None,
                        workspace_id,
                        profile_id,
                        bundle_id,
                        project_directory,
                        target_template_id,
                        additional_mounts,
                        // Local changes are never part of isolated creation;
                        // the compatibility field is intentionally ignored.
                        resource_allocation,
                        title,
                        session_title_override: None,
                    })
                    .await
            })??;
            Ok(Some(RegisteredDashboardSession {
                retry_launch: retry_launch.clone(),
                session: registered.session,
                remembered_container_size: registered.remembered_container_size,
                cancelled: cancelled.clone(),
            }))
        })();
        let Some(registered) = (match registered {
            Ok(registered) => registered,
            Err(error) => {
                report(
                    "creating session",
                    &updates,
                    DashboardIoUpdate::CreateSession(Box::new(
                        DashboardCreateSessionUpdate::Failed {
                            retry_launch: Box::new(retry_launch),
                            error: format!("{error:#}"),
                        },
                    )),
                );
                None
            }
        }) else {
            return;
        };
        let session_id = registered.session.id.clone();
        report(
            "creating session",
            &updates,
            DashboardIoUpdate::CreateSession(Box::new(DashboardCreateSessionUpdate::Registered(
                Box::new(registered),
            ))),
        );
        drop(guard);
        follow_daemon_creation(
            "creating session",
            session_id,
            &lifecycle_updates,
            cancelled,
        );
    });
}

/// Reports how a session the daemon has accepted finishes launching.
///
/// The daemon owns the launch from registration on, and it continues after
/// this terminal exits. Callers therefore release their exit guard before
/// calling this, so quitting never waits for a launch or its rollback.
fn follow_daemon_creation(
    operation: &str,
    session_id: String,
    lifecycle_updates: &UnboundedSender<DashboardLifecycleUpdate>,
    cancelled: Arc<AtomicBool>,
) {
    let result = mj_core::runtime::block_on(async {
        let mut daemon = daemon::connect_or_start().await?;
        daemon.wait_create_session(session_id.clone()).await?;
        Ok::<_, anyhow::Error>(LifecycleSuccess::Created)
    })
    .and_then(|result| result)
    .map_err(crate::pollers::DashboardLifecycleFailure::from);
    report(
        operation,
        lifecycle_updates,
        DashboardLifecycleUpdate {
            session_id,
            result,
            operation: cancelled,
        },
    );
}

/// Follow one query until the index settles. Dropping its guard cancels both
/// the debounce and any outstanding network wait; daemon-owned work survives.
pub(crate) fn spawn_wiki_search(
    request_id: u64,
    query: String,
    delay: Duration,
    updates: UnboundedSender<DashboardIoUpdate>,
) -> tokio_util::sync::DropGuard {
    let cancel = tokio_util::sync::CancellationToken::new();
    let guard = cancel.clone().drop_guard();
    tokio::spawn(async move {
        let answers = updates.clone();
        let live_answers = updates.clone();
        let live_query = query.clone();
        let task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(follow_wiki_search(
            delay,
            Duration::from_secs(5),
            move || {
                let query = query.clone();
                async move {
                    daemon::connect_or_start()
                        .await?
                        .wiki_search(query, WIKI_SEARCH_LIMIT)
                        .await
                }
            },
            move |result| {
                report(
                    "searching the session archive",
                    &answers,
                    DashboardIoUpdate::WikiRows { request_id, result },
                )
            },
            move || {
                let query = live_query.clone();
                async move {
                    if query.trim().is_empty() {
                        return Ok(Vec::new());
                    }
                    daemon::connect_or_start()
                        .await?
                        .session_text_search(query)
                        .await
                }
            },
            move |result| {
                report(
                    "searching live session conversations",
                    &live_answers,
                    DashboardIoUpdate::ResumeTextMatches { request_id, result },
                )
            },
        )));
        tokio::select! {
            _ = cancel.cancelled() => {},
            finished = task => {
                if let Err(error) = finished {
                    report("searching the session archive", &updates,
                        DashboardIoUpdate::WikiRows {
                            request_id,
                            result: Err(format!("session search task failed: {error}")),
                        });
                    report("searching live session conversations", &updates,
                        DashboardIoUpdate::ResumeTextMatches {
                            request_id,
                            result: Err(format!("session search task failed: {error}")),
                        });
                }
            }
        }
    });
    guard
}

async fn follow_wiki_search<F, Fut, L, LiveFut>(
    delay: Duration,
    refresh: Duration,
    mut search: F,
    mut answer: impl FnMut(std::result::Result<mj_client::daemon::WikiSearchPage, String>),
    mut live_search: L,
    mut live_answer: impl FnMut(std::result::Result<Vec<mj_client::daemon::SessionTextMatch>, String>),
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<mj_client::daemon::WikiSearchPage>>,
    L: FnMut() -> LiveFut,
    LiveFut: std::future::Future<Output = Result<Vec<mj_client::daemon::SessionTextMatch>>>,
{
    tokio::time::sleep(delay).await;
    loop {
        // Publish each independent answer as soon as it is ready. Neither
        // search can delay the other surface's results or hide its failure.
        let (repeat, ()) = tokio::join!(
            async {
                let result = search().await.map_err(|error| format!("{error:#}"));
                let repeat = result.as_ref().is_ok_and(|page| {
                    use mj_client::daemon::WikiIndexState;
                    page.status.state == WikiIndexState::Indexing
                        || (page.status.state == WikiIndexState::Ready && page.status.topping_up)
                });
                answer(result);
                repeat
            },
            async {
                live_answer(live_search().await.map_err(|error| format!("{error:#}")));
            }
        );
        if !repeat {
            return;
        }
        tokio::time::sleep(refresh).await;
    }
}

/// Search the conversations of the live sessions for the Sessions filter's
/// text, after the typing pause.
///
/// The wait and the daemon call are in a task, so the render loop never waits
/// on the index. A task whose request id is no longer the newest when it wakes
/// stops without asking the daemon, and the dashboard drops an answer that
/// names an older request.
pub(crate) fn spawn_sessions_text_search(
    request_id: u64,
    query: String,
    newest_request: Arc<AtomicU64>,
    updates: UnboundedSender<DashboardIoUpdate>,
) {
    newest_request.store(request_id, Ordering::Release);
    tokio::spawn(async move {
        tokio::time::sleep(WIKI_SEARCH_DEBOUNCE).await;
        if newest_request.load(Ordering::Acquire) != request_id {
            return;
        }
        let result = async {
            daemon::connect_or_start()
                .await?
                .session_text_search(query)
                .await
        }
        .await
        .map_err(|error| format!("{error:#}"));
        report(
            "searching session conversations",
            &updates,
            DashboardIoUpdate::SessionTextMatches { request_id, result },
        );
    });
}

/// How long the dialog waits after a keystroke before asking the index.
pub(crate) const WIKI_SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);

/// How many archived sessions one search asks for.
const WIKI_SEARCH_LIMIT: usize = 50;
/// How much of an archived transcript the preview pane asks for. It is a few
/// lines tall, so a whole briefing would be wasted work.
const WIKI_BRIEF_CHARS: usize = 4_000;

pub(crate) fn spawn_wiki_brief(wiki_id: String, updates: UnboundedSender<DashboardIoUpdate>) {
    tokio::spawn(async move {
        let result = async {
            daemon::connect_or_start()
                .await?
                .wiki_brief(wiki_id.clone(), WIKI_BRIEF_CHARS)
                .await
        }
        .await
        .map_err(|error| format!("{error:#}"));
        report(
            "reading an archived transcript",
            &updates,
            DashboardIoUpdate::WikiBrief { wiki_id, result },
        );
    });
}

/// How many messages of context the hit preview asks for on each side of a
/// matching message: enough to see what the match answered.
const WIKI_HITS_CONTEXT: usize = 1;
/// How much of one message the hit preview asks for. The pane is scrollable
/// but hit-centred, so a long message is shown around its match.
const WIKI_HITS_CHARS: usize = 2_000;

pub(crate) fn spawn_wiki_hits(
    wiki_id: String,
    query: String,
    updates: UnboundedSender<DashboardIoUpdate>,
) {
    tokio::spawn(async move {
        let result = async {
            daemon::connect_or_start()
                .await?
                .wiki_hits(
                    wiki_id.clone(),
                    query.clone(),
                    WIKI_HITS_CONTEXT,
                    WIKI_HITS_CHARS,
                )
                .await
        }
        .await
        .map_err(|error| format!("{error:#}"));
        report(
            "reading an archived transcript's matches",
            &updates,
            DashboardIoUpdate::WikiHits {
                wiki_id,
                query,
                result,
            },
        );
    });
}

/// Start a session from an archived transcript and follow it through creation
/// the same way a new session is followed.
pub(crate) fn spawn_dashboard_restore_session(
    request: mj_client::daemon::WikiRestoreRequest,
    retry_launch: DashboardAction,
    updates: UnboundedSender<DashboardIoUpdate>,
    lifecycle_updates: UnboundedSender<DashboardLifecycleUpdate>,
    tracker: CriticalOperationTracker,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = tracker.begin_cancellable("restoring archived session", cancelled.clone());
    tokio::task::spawn_blocking(move || {
        let registered = mj_core::runtime::block_on(async {
            daemon::connect_or_start()
                .await?
                .wiki_restore(request)
                .await
        })
        .and_then(|result| result);
        let registered = match registered {
            Ok(registered) => registered,
            Err(error) => {
                report(
                    "restoring archived session",
                    &updates,
                    DashboardIoUpdate::CreateSession(Box::new(
                        DashboardCreateSessionUpdate::Failed {
                            retry_launch: Box::new(retry_launch),
                            error: format!("{error:#}"),
                        },
                    )),
                );
                drop(guard);
                return;
            }
        };
        let session_id = registered.session.id.clone();
        report(
            "restoring archived session",
            &updates,
            DashboardIoUpdate::CreateSession(Box::new(DashboardCreateSessionUpdate::Registered(
                Box::new(RegisteredDashboardSession {
                    retry_launch,
                    session: registered.session,
                    remembered_container_size: registered.remembered_container_size,
                    cancelled: cancelled.clone(),
                }),
            ))),
        );
        drop(guard);
        follow_daemon_creation(
            "restoring archived session",
            session_id,
            &lifecycle_updates,
            cancelled,
        );
    });
}

/// Each disposable read owns a connection. Superseding the input closes that
/// connection, allowing the daemon to stop its executor and reap its children.
pub(crate) fn machine_request(
    action: mj_client::daemon::DaemonAction,
    cancelled: Arc<AtomicBool>,
) -> Result<mj_client::daemon::DaemonReply> {
    mj_core::runtime::block_on(async move {
        let request = async move {
            let mut client = daemon::connect_or_start().await?;
            client.request(action).await
        };
        tokio::pin!(request);
        loop {
            tokio::select! {
                result = &mut request => return result,
                _ = tokio::time::sleep(Duration::from_millis(10)) => {
                    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                        anyhow::bail!("machine request cancelled");
                    }
                }
            }
        }
    })?
}

#[cfg(test)]
mod wiki_search_tests {
    use super::*;
    use mj_client::daemon::{WikiIndexState, WikiSearchPage, WikiStatus};

    fn page(state: WikiIndexState, topping_up: bool) -> WikiSearchPage {
        WikiSearchPage {
            status: WikiStatus { state, topping_up },
            rows: Vec::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn one_query_publishes_each_answer_until_the_index_settles() {
        let mut replies = std::collections::VecDeque::from([
            Ok(page(WikiIndexState::Indexing, true)),
            Ok(page(WikiIndexState::Ready, true)),
            Ok(page(WikiIndexState::Ready, true)),
            Ok(page(WikiIndexState::Ready, false)),
        ]);
        let started = tokio::time::Instant::now();
        let mut answers = Vec::new();
        follow_wiki_search(
            Duration::from_millis(200),
            Duration::from_secs(5),
            || std::future::ready(replies.pop_front().expect("no searches after ready")),
            |reply| answers.push((tokio::time::Instant::now() - started, reply.unwrap().status)),
            || std::future::ready(Ok(Vec::new())),
            |_| {},
        )
        .await;
        assert!(replies.is_empty());
        assert_eq!(answers.len(), 4);
        assert_eq!(answers[0].0, Duration::from_millis(200));
        assert_eq!(answers[3].0, Duration::from_millis(15_200));
        assert!(!answers[3].1.topping_up);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_and_incompatible_searches_publish_once_and_stop() {
        for reply in [
            Err(anyhow::anyhow!("database failed")),
            Ok(page(WikiIndexState::VersionMismatch, true)),
        ] {
            let mut reply = Some(reply);
            let mut answers = Vec::new();
            follow_wiki_search(
                Duration::ZERO,
                Duration::from_secs(5),
                || std::future::ready(reply.take().expect("no retry after failure or mismatch")),
                |answer| answers.push(answer),
                || std::future::ready(Ok(Vec::new())),
                |_| {},
            )
            .await;
            assert_eq!(answers.len(), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancelling_a_query_during_debounce_stops_its_background_task() {
        let (updates, mut replies) = tokio::sync::mpsc::unbounded_channel();
        let query = spawn_wiki_search(17, "quokka".into(), Duration::from_secs(60), updates);
        tokio::task::yield_now().await;
        drop(query);
        // Closure of the sender proves its supervising task and search task exit.
        assert!(replies.recv().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn live_and_history_answers_arrive_independently_even_when_one_fails() {
        for history_is_slow in [true, false] {
            let started = tokio::time::Instant::now();
            let mut history_at = None;
            let mut live_at = None;
            follow_wiki_search(
                Duration::ZERO,
                Duration::from_secs(5),
                || async {
                    if history_is_slow {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        Err(anyhow::anyhow!("history failed"))
                    } else {
                        Ok(page(WikiIndexState::Ready, false))
                    }
                },
                |reply| {
                    assert_eq!(reply.is_err(), history_is_slow);
                    history_at = Some(started.elapsed());
                },
                || async {
                    if history_is_slow {
                        Ok(Vec::new())
                    } else {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        Err(anyhow::anyhow!("live search failed"))
                    }
                },
                |reply| {
                    assert_eq!(reply.is_err(), !history_is_slow);
                    live_at = Some(started.elapsed());
                },
            )
            .await;
            let (slow, fast) = if history_is_slow {
                (history_at, live_at)
            } else {
                (live_at, history_at)
            };
            assert_eq!(slow, Some(Duration::from_secs(30)));
            assert_eq!(fast, Some(Duration::ZERO));
        }
    }
}

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
        let revision = daemon
            .runtime_snapshot(String::new(), 0, true)
            .await?
            .revision;
        Ok(WorkspaceManagementResult {
            revision,
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
    spawn_io(
        "load workspace pane sizes",
        updates,
        move || {
            workspace_ids
                .into_iter()
                .map(|workspace_id| {
                    let sizes = mj_controller::database::load_workspace_pane_sizes(&workspace_id)?;
                    Ok((workspace_id, sizes))
                })
                .collect()
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
    spawn_io(
        "load workspace layouts",
        updates,
        move || {
            workspace_ids
                .into_iter()
                .map(|workspace_id| {
                    let layout = mj_controller::database::load_workspace_layout(&workspace_id)?;
                    Ok((workspace_id, layout))
                })
                .collect()
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
            let revision = daemon
                .runtime_snapshot(String::new(), 0, true)
                .await?
                .revision;
            Ok(WorkspaceManagementResult {
                revision,
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
            let revision = daemon
                .runtime_snapshot(String::new(), 0, true)
                .await?
                .revision;
            Ok(WorkspaceManagementResult {
                revision,
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
            daemon.close_workspace(workspace_id.clone()).await?;
            let revision = daemon
                .runtime_snapshot(String::new(), 0, true)
                .await?
                .revision;
            Ok(WorkspaceManagementResult {
                revision,
                entries: load_workspace_management_entries(&mut daemon).await?,
                select_workspace: None,
                deleted_workspace_id: Some(workspace_id),
            })
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

pub(crate) fn spawn_workspace_delete(
    generation: u64,
    workspace_id: String,
    force: bool,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    let deleted_workspace_id = workspace_id.clone();
    spawn_workspace_management_operation(
        generation,
        if force {
            "force deleting workspace"
        } else {
            "deleting workspace"
        },
        updates,
        Some(tracker),
        async move {
            let mut daemon = daemon::connect_or_start().await?;
            if force {
                daemon.force_delete_workspace(workspace_id).await?;
            } else {
                daemon.delete_workspace(workspace_id).await?;
            }
            let revision = daemon
                .runtime_snapshot(String::new(), 0, true)
                .await?
                .revision;
            Ok(WorkspaceManagementResult {
                revision,
                entries: load_workspace_management_entries(&mut daemon).await?,
                select_workspace: None,
                deleted_workspace_id: Some(deleted_workspace_id),
            })
        },
    )
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
            let revision = daemon
                .runtime_snapshot(String::new(), 0, true)
                .await?
                .revision;
            Ok(WorkspaceManagementResult {
                revision,
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

/// Discovers advertised reviewer choices in a supervised task. The TUI
/// drops replies whose generation no longer matches the current draft.
pub(crate) fn spawn_review_settings_discovery(
    control: SessionManagerControl,
    request: mj_controller::review_settings::ReviewDiscoveryRequest,
    generation: u64,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> Arc<AtomicBool> {
    let profile_id = request.profile.clone();
    let model = request.model.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = tracker.begin_cancellable("loading review choices", cancelled.clone());
    let worker_cancelled = cancelled.clone();
    tokio::spawn(async move {
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
        let discovery = mj_controller::review_settings::discover_review_settings(
            control,
            request,
            worker_cancelled,
            progress_tx,
        );
        tokio::pin!(discovery);
        let result = loop {
            tokio::select! {
                // Drain ready choices before final completion so a queued progress
                // event can never arrive after the final discovery result.
                biased;
                Some(choices) = progress_rx.recv() => {
                    report(
                        "loading review choices",
                        &updates,
                        DashboardIoUpdate::ReviewSettingsChoices {
                            generation,
                            profile_id: profile_id.clone(),
                            model: model.clone(),
                            choices,
                        },
                    );
                }
                result = &mut discovery => break result,
            }
        }
        .map(|outcome| match outcome {
            mj_controller::review_settings::ReviewDiscoveryOutcome::Available {
                choices,
                cleanup_warning,
            } => ReviewSettingsDiscoveryResult::Available {
                choices: review_settings_choices(choices),
                cleanup_warning,
            },
            mj_controller::review_settings::ReviewDiscoveryOutcome::Unavailable => {
                ReviewSettingsDiscoveryResult::Unavailable
            }
        })
        .map_err(|error| format!("{error:#}"));
        report(
            "loading review choices",
            &updates,
            DashboardIoUpdate::ReviewSettingsDiscovered {
                generation,
                profile_id,
                model,
                result,
            },
        );
        drop(guard);
    });
    cancelled
}

pub(crate) fn review_settings_choices(
    choices: mj_controller::review_settings::ReviewCapabilityChoices,
) -> ReviewSettingsChoices {
    ReviewSettingsChoices {
        model_choices: choices.model_choices,
        effort_choices: choices.effort_choices,
        effort_capabilities_discovered: choices.effort_capabilities_discovered,
    }
}

pub(crate) fn spawn_setup_discovery(
    generation: u64,
    scope: DetectScope,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_cancellable_io(
        tracker,
        match scope {
            DetectScope::Profiles => "detecting agent profiles",
            DetectScope::Runtimes => "detecting container runtimes",
        },
        updates,
        move |cancelled| {
            use mj_controller::setup::{self, DEFAULT_IMAGE};
            let executor =
                CancellableProcessExecutor::new(cancelled).with_deadline(Duration::from_secs(30));
            let mut config = Config::default();
            let mut rejected_runtimes = Vec::new();
            match scope {
                DetectScope::Profiles => {
                    config = setup::profiles_config(&setup::discover_profiles(&executor));
                }
                DetectScope::Runtimes => {
                    for runtime in setup::discover_runtimes(&executor) {
                        if runtime.usable {
                            let (id, target) =
                                setup::local_runtime_target(runtime.kind, DEFAULT_IMAGE);
                            config.targets.insert(id.to_owned(), target);
                        } else {
                            rejected_runtimes.push(RejectedRuntime {
                                label: runtime.kind.label().to_owned(),
                                detail: runtime.detail,
                                remediation: runtime.remediation,
                            });
                        }
                    }
                    #[cfg(unix)]
                    config.targets.insert(
                        "localhost".into(),
                        mj_core::config::TargetTemplate::LocalBare,
                    );
                }
            }
            Ok(SetupDetection {
                scope,
                config,
                rejected_runtimes,
            })
        },
        move |result| DashboardIoUpdate::SetupDiscovered { generation, result },
    )
}

pub(crate) fn spawn_setup_save(
    generation: u64,
    original: String,
    updated: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    spawn_critical_io(
        tracker,
        "saving setup",
        updates,
        move || {
            let state = mj_controller::database::load_state()?;
            save_setup_at(&mj_core::config::config_path(), &original, &updated, &state)
        },
        move |result| DashboardIoUpdate::SetupSaved { generation, result },
    )
}

pub(crate) fn save_setup_at(
    path: &std::path::Path,
    original: &str,
    updated: &str,
    state: &State,
) -> Result<Config> {
    let original: serde_json::Value = serde_json::from_str(original)?;
    let updated_config: Config = serde_json::from_str(updated)?;
    updated_config.validate()?;
    let updated = serde_json::to_value(updated_config)?;
    Config::update_to(path, |config| {
        // The editor includes implicit local defaults. Treat those same defaults
        // as the merge base when they have not been written to disk yet.
        let defaults = Config::default().with_local_targets();
        // Compare in the stored shape the editor works in, where a runtime
        // names its machine, rather than in the fused in-memory shape.
        let stored_defaults = serde_json::to_value(&defaults)?;
        for (id, target) in defaults.targets {
            if !config.targets.contains_key(&id)
                && original["targets"][&id] == stored_defaults["targets"][&id]
            {
                config.targets.insert(id, target);
            }
        }
        let current = serde_json::to_value(&*config)?;
        let merged = merge_setup_edit(Some(&original), Some(&updated), Some(&current), "Setup")?
            .context("setup cannot remove the configuration")?;
        let next = serde_json::from_value(merged)?;
        state.validate_setup_update(config, &next)?;
        *config = next;
        Ok(())
    })
    .map(|(config, ())| config)
}

/// Apply only fields the dialog changed; refuse conflicting concurrent edits.
pub(crate) fn merge_setup_edit(
    original: Option<&serde_json::Value>,
    updated: Option<&serde_json::Value>,
    current: Option<&serde_json::Value>,
    path: &str,
) -> Result<Option<serde_json::Value>> {
    use serde_json::Value;
    if original == updated || updated == current {
        return Ok(current.cloned());
    }
    if original == current {
        return Ok(updated.cloned());
    }
    if updated.is_some_and(Value::is_object)
        && original.is_none_or(Value::is_object)
        && current.is_some_and(Value::is_object)
    {
        let keys = original
            .into_iter()
            .chain(updated)
            .chain(current)
            .flat_map(|value| value.as_object().unwrap().keys())
            .collect::<BTreeSet<_>>();
        let mut merged = serde_json::Map::new();
        for key in keys {
            if let Some(value) = merge_setup_edit(
                original.and_then(|v| v.get(key)),
                updated.and_then(|v| v.get(key)),
                current.and_then(|v| v.get(key)),
                &format!("{path} / {key}"),
            )? {
                merged.insert(key.clone(), value);
            }
        }
        return Ok(Some(Value::Object(merged)));
    }
    bail!("{path} changed in another client. Reopen Setup to edit the latest value.")
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
    controller: &Controller,
    session_id: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    let config = controller.config.clone();
    let session = controller.state.sessions.get(&session_id).cloned();
    let source_controller = Controller {
        config,
        state: State {
            sessions: session
                .map(|session| [(session_id.clone(), session)].into_iter().collect())
                .unwrap_or_default(),
            ..State::default()
        },
    };
    let reported_session_id = session_id.clone();
    spawn_cancellable_io(
        tracker,
        format!("resolving project for {}", short_id(&session_id)),
        updates,
        move |cancelled| {
            let executor =
                CancellableProcessExecutor::new(cancelled).with_deadline(Duration::from_secs(8));
            source_controller.resolve_session_project_source(&session_id, &executor)
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
    pub(crate) kind: SessionOperationKind,
    pub(crate) cancelled: Arc<AtomicBool>,
    pub(crate) updates: UnboundedSender<LifecycleUpdate>,
}

/// Runs one session lifecycle operation on a blocking task.
///
/// Every one of them reloads the controller so it acts on durable state, then
/// answers on the lifecycle channel whatever happens. The daemon owns
/// lifecycle/recovery serialization.
pub(crate) fn spawn_lifecycle_operation(
    request: LifecycleOperationRequest,
    tracker: CriticalOperationTracker,
    work: impl FnOnce(&mut Controller, Arc<AtomicBool>) -> Result<LifecycleSuccess> + Send + 'static,
) {
    let LifecycleOperationRequest {
        session_id,
        kind,
        cancelled,
        updates,
    } = request;
    let guard = tracker.begin_cancellable(
        format!(
            "{} session {}",
            kind.label().to_ascii_lowercase(),
            short_id(&session_id)
        ),
        cancelled.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let result = (|| -> Result<LifecycleSuccess> {
            let mut controller = Controller::load()?;
            work(&mut controller, cancelled)
        })()
        .map_err(|error| format!("{error:#}"));
        report(
            "session lifecycle operation",
            &updates,
            LifecycleUpdate {
                session_id,
                result,
                deferred_cleanup: false,
            },
        );
        drop(guard);
    });
}

pub(crate) fn spawn_materialized_session_projection(
    materialized: MaterializedSession,
    viewed_through_event_ordinal: u64,
    previous: mj_tui::MaterializedProjectionCache,
    updates: UnboundedSender<DashboardIoUpdate>,
    permits: Arc<tokio::sync::Semaphore>,
) {
    let session_id = materialized.session_id.clone();
    tokio::spawn(async move {
        let result = match permits.acquire_owned().await {
            Ok(permit) => {
                let result = tokio::task::spawn_blocking(move || {
                    PreparedMaterializedSessionDetail::from_materialized(
                        materialized,
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

pub(crate) fn spawn_stored_session_summary(
    session_id: String,
    viewed_through_event_ordinal: u64,
    updates: UnboundedSender<DashboardIoUpdate>,
) {
    let reported_session_id = session_id.clone();
    spawn_io(
        "load stored session summary",
        updates,
        move || {
            let summary = mj_controller::database::load_materialized_session_summary(&session_id)?
                .with_context(|| format!("session {session_id} has no stored projection"))?;
            Ok(PreparedMaterializedSessionSummary::from_materialized(
                summary,
                viewed_through_event_ordinal,
            ))
        },
        move |result| DashboardIoUpdate::StoredSessionSummary {
            session_id: reported_session_id,
            result,
        },
    );
}

pub(crate) fn spawn_lifecycle_reload(
    reload: LifecycleReload,
    workspace_id: String,
    client_id: String,
    updates: UnboundedSender<DashboardIoUpdate>,
) {
    spawn_io(
        "reload lifecycle state",
        updates,
        move || {
            let mut controller = Controller::load()?;
            crate::dashboard::retain_workspace_sessions(
                &mut controller,
                &workspace_id,
                &client_id,
            )?;
            Ok(controller)
        },
        move |result| {
            DashboardIoUpdate::LifecycleReloaded(Box::new(LifecycleReloaded { reload, result }))
        },
    );
}

pub(crate) fn spawn_dashboard_rename(
    session_id: String,
    title: String,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    let renamed_session_id = session_id.clone();
    let requested_title = title.clone();
    spawn_critical_async(
        tracker,
        format!("renaming session {}", short_id(&session_id)),
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            daemon::connect_or_start()
                .await?
                .set_session_title(renamed_session_id, requested_title)
                .await
        },
        move |result| DashboardIoUpdate::RenameSession {
            session_id,
            title,
            result,
        },
    );
}

pub(crate) fn spawn_startup_prompt(
    session_id: String,
    text: String,
    inherited_draft: Option<String>,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    let queued_session_id = session_id.clone();
    let queued_text = text.clone();
    spawn_critical_async(
        tracker,
        format!("queueing a prompt for session {}", short_id(&session_id)),
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            daemon::connect_or_start()
                .await?
                .queue_startup_prompt(queued_session_id, queued_text, inherited_draft)
                .await
        },
        move |result| DashboardIoUpdate::StartupPromptQueued {
            session_id,
            text,
            result,
        },
    );
}

pub(crate) struct ConfigRenameRequest {
    pub(crate) what: String,
    pub(crate) old_id: String,
    pub(crate) new_id: String,
    pub(crate) profile: bool,
    pub(crate) workspace_id: String,
    pub(crate) client_id: String,
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
        workspace_id,
        client_id,
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
            tokio::task::spawn_blocking(move || {
                let mut controller = Controller::load()?;
                crate::dashboard::retain_workspace_sessions(
                    &mut controller,
                    &workspace_id,
                    &client_id,
                )?;
                Ok::<_, anyhow::Error>(controller)
            })
            .await
            .context("configuration reload task panicked")?
        },
        move |result| DashboardIoUpdate::ConfigRename { what, result },
    );
}

/// What the container editor asks the controller to persist.
pub(crate) struct ContainerSettingsRequest {
    pub(crate) session_id: String,
    pub(crate) cpus: Option<String>,
    pub(crate) memory: Option<String>,
    pub(crate) additional_mounts: Vec<mj_controller::targets::AdditionalMount>,
    pub(crate) mount_history: Vec<std::path::PathBuf>,
    pub(crate) workspace_id: String,
    pub(crate) client_id: String,
}

pub(crate) fn spawn_dashboard_container_settings(
    request: ContainerSettingsRequest,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    let session_id = request.session_id.clone();
    spawn_critical_io(
        tracker,
        format!("saving container settings for {}", short_id(&session_id)),
        updates,
        move || {
            let ContainerSettingsRequest {
                session_id,
                cpus,
                memory,
                additional_mounts,
                mount_history,
                workspace_id,
                client_id,
            } = request;
            mj_core::runtime::block_on(async {
                daemon::connect_or_start()
                    .await?
                    .set_session_container_settings(
                        session_id,
                        cpus,
                        memory,
                        additional_mounts,
                        mount_history,
                    )
                    .await
            })??;
            // Return a fresh durable snapshot so the dashboard can update its
            // state without synchronously reloading the database while it is
            // applying the worker result.
            let mut controller = Controller::load()?;
            crate::dashboard::retain_workspace_sessions(
                &mut controller,
                &workspace_id,
                &client_id,
            )?;
            Ok(controller)
        },
        move |result| DashboardIoUpdate::ContainerSettings { session_id, result },
    );
}

/// Persist everything one detach produces: the read receipt and the unsent
/// draft. They describe the same moment and the same row, so one task keeps
/// them together and gives the quit path a single handle to await.
pub(crate) fn spawn_detached_session_state_persist(
    client_id: String,
    workspace_id: String,
    session_id: String,
    event_ordinal: u64,
    draft: DetachedSessionDraft,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) -> JoinHandle<()> {
    let persisted_session_id = session_id.clone();
    spawn_critical_async(
        tracker,
        format!("saving draft for {}", short_id(&session_id)),
        updates,
        SAVE_ACK_TIMEOUT,
        async move {
            daemon::connect_or_start()
                .await?
                .persist_detached_session_state(
                    client_id,
                    workspace_id,
                    persisted_session_id,
                    event_ordinal,
                    std::process::id(),
                    draft,
                )
                .await
        },
        move |result| DashboardIoUpdate::DetachedSessionState { session_id, result },
    )
}

pub(crate) fn spawn_read_receipt_persist(
    client_id: String,
    workspace_id: String,
    session_id: String,
    through: u64,
    retry_delay: Duration,
    updates: UnboundedSender<DashboardIoUpdate>,
    tracker: CriticalOperationTracker,
) {
    let persisted_session_id = session_id.clone();
    spawn_critical_async(
        tracker,
        format!("saving read status for {}", short_id(&session_id)),
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

pub(crate) fn spawn_clipboard_read(updates: UnboundedSender<DashboardIoUpdate>) -> JoinHandle<()> {
    spawn_io(
        "read clipboard",
        updates,
        mj_chat::clipboard::read_text,
        DashboardIoUpdate::ClipboardText,
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
    spawn_critical_io(
        tracker,
        "creating bundle",
        updates,
        move || {
            // Load fresh so a concurrent background save (e.g. an import
            // apply) is not clobbered by a stale UI-time config snapshot.
            let created = mj_controller::controller::create_bundle_from_sources(&sources)?;
            Ok(CreatedBundleUpdate {
                config: created.config,
                bundle_id: created.bundle_id,
            })
        },
        |result| DashboardIoUpdate::CreatedBundle {
            result: Box::new(result),
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
            Config::update(|config| {
                if let Some(existing) = config.bundles.get(&session.bundle_id) {
                    anyhow::ensure!(
                        existing == &bundle,
                        "bundle {:?} changed during import; retry the import",
                        session.bundle_id
                    );
                } else {
                    config
                        .bundles
                        .insert(session.bundle_id.clone(), bundle.clone());
                }
                Ok(())
            })?;
            persist_imported_session(&session)?;
            Ok(ImportedDashboardSessionApply {
                harness: imported.harness,
                native_session_id: pending.native_session_id,
                bundle_id: session.bundle_id.clone(),
                bundle,
                session,
            })
        },
        |result| DashboardIoUpdate::ImportedSessionApplied {
            result: Box::new(result),
        },
    );
}

pub(crate) fn checkpoint_archive_targets(controller: &Controller) -> BTreeMap<String, PathBuf> {
    controller
        .state
        .sessions
        .values()
        .filter(|session| session.state == SessionState::Stopped)
        .filter_map(|session| {
            session
                .checkpoint
                .as_ref()
                .map(|checkpoint| (session.id.clone(), checkpoint.archive_path.clone()))
        })
        .collect()
}

pub(crate) fn spawn_checkpoint_archive_size_refresh(
    generation: u64,
    targets: BTreeMap<String, PathBuf>,
    updates: UnboundedSender<DashboardIoUpdate>,
) {
    tokio::task::spawn_blocking(move || {
        let sizes = targets
            .into_iter()
            .map(|(session_id, path)| {
                let size = match std::fs::metadata(&path) {
                    Ok(metadata) => Some(metadata.len()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => {
                        tracing::warn!(path = %path.display(), %error, "could not read checkpoint archive size");
                        None
                    }
                };
                (session_id, size)
            })
            .collect();
        report(
            "checkpoint archive sizes",
            &updates,
            DashboardIoUpdate::CheckpointArchiveSizes { generation, sizes },
        );
    });
}

/// Registering a session and provisioning it are one job with two answers: the
/// dashboard shows the session as soon as it exists, then follows the launch,
/// so this stays separate from [`spawn_lifecycle_operation`].
pub(crate) fn spawn_dashboard_create_session(
    action: DashboardAction,
    go_save: Option<(std::path::PathBuf, mj_core::go::GoRecipe, bool)>,
    updates: UnboundedSender<DashboardIoUpdate>,
    lifecycle_updates: UnboundedSender<LifecycleUpdate>,
    tracker: CriticalOperationTracker,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let guard = tracker.begin_cancellable("creating session", cancelled.clone());
    tokio::task::spawn_blocking(move || {
        let retry_launch = action.clone();
        let DashboardAction::CreateSession {
            create_managed_worktree,
            mjolnir_subagents,
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
            let controller = Controller::load()?;
            let executor = CancellableProcessExecutor::new(cancelled.clone())
                .with_deadline(Duration::from_secs(30));
            if project_directory.is_none() {
                let bundle = controller
                    .config
                    .bundles
                    .get(&bundle_id)
                    .context("unknown bundle")?;
                let repairs = mj_core::local_git::repository_remote_repairs(bundle, &executor)?;
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
                resolve_remote_repositories(
                    &controller.config,
                    &bundle_id,
                    &target_template_id,
                    &executor,
                )?;
            }
            if cancelled.load(Ordering::Acquire) {
                bail!("operation cancelled");
            }
            let title = format!(
                "{} via {profile_id}",
                project_directory
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| bundle_id.clone())
            );
            let registered = mj_core::runtime::block_on(async {
                daemon::connect_or_start()
                    .await?
                    .start_create_session(daemon::CreateSessionRequest {
                        mjolnir_subagents,
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
        let result = mj_core::runtime::block_on(async {
            let mut daemon = daemon::connect_or_start().await?;
            daemon.wait_create_session(session_id.clone()).await?;
            Ok::<_, anyhow::Error>(LifecycleSuccess::Created)
        })
        .and_then(|result| result)
        .map_err(|error| format!("{error:#}"));
        report(
            "creating session",
            &lifecycle_updates,
            LifecycleUpdate {
                session_id,
                result,
                deferred_cleanup: false,
            },
        );
        drop(guard);
    });
}

/// Search the SessionWiki index for the resume dialog, after `delay`.
///
/// The wait is in the task rather than in a timer on the event loop: each
/// keystroke starts one, and a task whose request id is no longer the newest
/// when it wakes stops without asking the daemon. The dialog drops any answer
/// that names an older request as well, because two searches can still
/// overlap. The same task serves the repeats a still-building or still-syncing
/// index asks for, with their own longer waits.
pub(crate) fn spawn_wiki_search(
    request_id: u64,
    query: String,
    delay: Duration,
    newest_request: Arc<AtomicU64>,
    updates: UnboundedSender<DashboardIoUpdate>,
) {
    newest_request.store(request_id, Ordering::Release);
    tokio::spawn(async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if newest_request.load(Ordering::Acquire) != request_id {
            return;
        }
        let result = async {
            daemon::connect_or_start()
                .await?
                .wiki_search(query, WIKI_SEARCH_LIMIT)
                .await
        }
        .await
        .map_err(|error| format!("{error:#}"));
        report(
            "searching the session archive",
            &updates,
            DashboardIoUpdate::WikiRows { request_id, result },
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
    lifecycle_updates: UnboundedSender<LifecycleUpdate>,
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
        let result = mj_core::runtime::block_on(async {
            let mut daemon = daemon::connect_or_start().await?;
            daemon.wait_create_session(session_id.clone()).await?;
            Ok::<_, anyhow::Error>(LifecycleSuccess::Created)
        })
        .and_then(|result| result)
        .map_err(|error| format!("{error:#}"));
        report(
            "restoring archived session",
            &lifecycle_updates,
            LifecycleUpdate {
                session_id,
                result,
                deferred_cleanup: false,
            },
        );
        drop(guard);
    });
}

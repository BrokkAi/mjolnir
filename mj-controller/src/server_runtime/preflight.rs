use super::*;

/// Cancel the shared subprocess executor when its request owner goes away.
pub(super) struct ProcessCancellationGuard(pub(super) Arc<AtomicBool>);

impl Drop for ProcessCancellationGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Perform the preflight for a new session.
///
/// This runs on the blocking task owned by the phone server. Bare targets
/// need the same directory-and-Git-HEAD validation as the dashboard. Isolated
/// targets resolve every bundle repository to a network source and report the
/// exact fetch branch and publication destinations. Local working-tree
/// contents are never copied for this path.
#[cfg(test)]
pub(super) fn run_new_preflight(
    config: Config,
    bundle_id: String,
    target_id: String,
    project_directory: Option<PathBuf>,
) -> Result<crate::server::PreflightNew> {
    run_new_preflight_with_cancellation(
        config,
        bundle_id,
        target_id,
        project_directory,
        Arc::new(AtomicBool::new(false)),
        Vec::new(),
    )
}

/// Run one resume preflight on its own task, like a new-session preflight:
/// the disk work stays off the feed loop, a disconnected browser cancels it,
/// and the task is supervised by the same `JoinSet`.
pub(super) fn spawn_resume_preflight(
    jobs: &mut tokio::task::JoinSet<()>,
    controller: &Controller,
    request: crate::server::ResumePreflightRequest,
    termination: &tokio_util::sync::CancellationToken,
) {
    let crate::server::ResumePreflightRequest {
        session_id,
        target_id,
        mut reply,
    } = request;
    let config = controller.config.clone();
    let session = controller.state.sessions.get(&session_id).cloned();
    let termination = termination.clone();
    jobs.spawn(async move {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation_guard = ProcessCancellationGuard(cancelled.clone());
        let mut blocking = tokio::task::spawn_blocking(move || {
            run_resume_preflight(config, session, &target_id, cancelled)
        });
        let answer = tokio::select! {
            biased;
            _ = termination.cancelled() => None,
            _ = reply.closed() => None,
            answer = &mut blocking => Some(answer),
        };
        let Some(answer) = answer else {
            drop(cancellation_guard);
            if let Err(error) = blocking.await {
                tracing::warn!(%error, "cancelled phone resume preflight task failed");
            }
            return;
        };
        let answer = answer.map_err(|error| {
            tracing::warn!(%error, "phone resume preflight task failed");
            PreflightFailure::Controller(format!("resume preflight task failed: {error}"))
        });
        if reply.send(answer).is_err() {
            tracing::debug!("phone resume preflight reply dropped after client disconnect");
        }
    });
}

/// Answer one path-completion request on its own task.
///
/// Listing a directory can mean an SSH round trip, so it stays off the feed
/// loop and runs under the same supervision, cap and cancellation as a
/// preflight. A browser that abandons the request, or a controller that is
/// shutting down, stops the child process rather than waiting for it.
pub(super) fn spawn_path_completion(
    jobs: &mut tokio::task::JoinSet<()>,
    config: &Config,
    request: crate::server::PathCompletionRequest,
    termination: &tokio_util::sync::CancellationToken,
) {
    let crate::server::PathCompletionRequest {
        host,
        prefix,
        kind,
        mut reply,
    } = request;
    let config = config.clone();
    let termination = termination.clone();
    jobs.spawn(async move {
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation_guard = ProcessCancellationGuard(cancelled.clone());
        let mut blocking = tokio::task::spawn_blocking(move || {
            let executor =
                CancellableProcessExecutor::new(cancelled).with_deadline(Duration::from_secs(5));
            config_only_controller(config).complete_path(&host, &prefix, kind, &executor)
        });
        let answer = tokio::select! {
            biased;
            _ = termination.cancelled() => None,
            _ = reply.closed() => None,
            answer = &mut blocking => Some(answer),
        };
        let Some(answer) = answer else {
            drop(cancellation_guard);
            if let Err(error) = blocking.await {
                tracing::warn!(%error, "cancelled phone path completion task failed");
            }
            return;
        };
        let answer = match answer {
            Ok(answer) => answer.map_err(|error| format!("{error:#}")),
            Err(error) => {
                tracing::warn!(%error, "phone path completion task failed");
                Err(format!("path completion task failed: {error}"))
            }
        };
        if reply.send(answer).is_err() {
            tracing::debug!("phone path completion reply dropped after client disconnect");
        }
    });
}

/// What resuming this session on this target does to its repository content.
///
/// Anything but a local-checkout conversion is `Ready` and reads nothing:
/// the compatibility gate is a pure function, so a browser may ask about
/// every destination it offers. A conversion that cannot be planned reports
/// the plan's own message, which is what tells a person to add a remote or
/// commit a submodule.
pub(super) fn run_resume_preflight(
    config: Config,
    session: Option<mj_core::state::SessionRecord>,
    target_id: &str,
    cancelled: Arc<AtomicBool>,
) -> crate::server::PreflightResume {
    let Some(session) = session else {
        return crate::server::PreflightResume::Unavailable {
            detail: "this session is no longer available".to_owned(),
        };
    };
    match crate::controller::resume_compatibility(&session, &config, target_id) {
        Err(reason) => crate::server::PreflightResume::Unavailable { detail: reason },
        Ok(plan) if plan != crate::controller::ResumePlan::RawToWorkspace => {
            crate::server::PreflightResume::Ready
        }
        Ok(_) => {
            let executor =
                CancellableProcessExecutor::new(cancelled).with_deadline(Duration::from_secs(30));
            match crate::controller::raw_conversion_preview_for(&session, &config, &executor) {
                Err(error) => crate::server::PreflightResume::Unavailable {
                    detail: format!("{error:#}"),
                },
                Ok(mut preview) => {
                    // Credentials never reach a browser, here as everywhere
                    // else a repository URL is published.
                    preview.fetch_url = display_url(&preview.fetch_url);
                    preview.push_urls = preview
                        .push_urls
                        .iter()
                        .map(|url| display_url(url))
                        .collect();
                    crate::server::PreflightResume::ConvertingRawCheckout {
                        preview: Box::new(preview),
                    }
                }
            }
        }
    }
}

pub(super) fn run_new_preflight_with_cancellation(
    config: Config,
    bundle_id: String,
    target_id: String,
    project_directory: Option<PathBuf>,
    cancelled: Arc<AtomicBool>,
    remote_repairs: Vec<mj_core::local_git::LocalRemoteRepair>,
) -> Result<crate::server::PreflightNew> {
    let executor =
        CancellableProcessExecutor::new(cancelled).with_deadline(Duration::from_secs(30));
    if !remote_repairs.is_empty() {
        let target = config.targets.get(&target_id).context("unknown target")?;
        anyhow::ensure!(
            !is_bare_project_target(target) && project_directory.is_none(),
            "remote repair requires an isolated target"
        );
        let bundle = config.bundles.get(&bundle_id).context("unknown bundle")?;
        mj_core::local_git::apply_repository_remote_repairs(bundle, &remote_repairs, &executor)?;
    }
    run_new_preflight_with_executor(config, bundle_id, target_id, project_directory, &executor)
}

pub(super) fn run_new_preflight_with_executor(
    config: Config,
    bundle_id: String,
    target_id: String,
    project_directory: Option<PathBuf>,
    executor: &impl CommandExecutor,
) -> Result<crate::server::PreflightNew> {
    let target_is_bare = config
        .targets
        .get(&target_id)
        .with_context(|| format!("unknown target template {target_id:?}"))
        .map(is_bare_project_target)?;
    if target_is_bare {
        let directory =
            project_directory.context("project directory is required for a bare target")?;
        let controller = config_only_controller(config);
        let directory = controller.resolve_project_directory(&target_id, &directory, executor)?;
        let managed_worktree =
            controller.managed_worktree_options(&target_id, &directory, executor)?;
        return Ok(crate::server::PreflightNew {
            managed_worktree,
            project_directory: Some(directory),
            remote_repairs: Vec::new(),
            dirty_repositories: Vec::new(),
            remote_repositories: Vec::new(),
            local_changes_excluded: false,
        });
    }
    if project_directory.is_some() {
        bail!("project directory is unsupported for this target");
    }

    let bundle = config.bundles.get(&bundle_id).context("unknown bundle")?;
    let repairs = mj_core::local_git::repository_remote_repairs(bundle, executor)?;
    if !repairs.is_empty() {
        return Ok(crate::server::PreflightNew {
            managed_worktree: Default::default(),
            project_directory: None,
            remote_repairs: repairs,
            dirty_repositories: Vec::new(),
            remote_repositories: Vec::new(),
            local_changes_excluded: true,
        });
    }
    let remote_repositories = bundle
        .repositories
        .iter()
        .map(|repository| {
            let source = resolve_repository(repository, executor)
                .with_context(|| format!("repository {:?}", repository.id))?;
            let default_branch = default_branch(&source, executor)
                .with_context(|| format!("repository {:?}", repository.id))?;
            Ok(crate::server::PreflightRepository {
                id: repository.id.clone(),
                fetch_url: display_url(&source.fetch_url),
                default_branch,
                push_urls: source
                    .push_urls
                    .iter()
                    .map(|url| display_url(url))
                    .collect(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(crate::server::PreflightNew {
        managed_worktree: Default::default(),
        project_directory: None,
        remote_repairs: Vec::new(),
        dirty_repositories: Vec::new(),
        remote_repositories,
        local_changes_excluded: true,
    })
}

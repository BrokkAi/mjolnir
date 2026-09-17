use super::*;

pub fn quota_refresh_profiles(controller: &Controller) -> Vec<QuotaRefreshRequest> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    controller
        .config
        .enabled_profiles()
        .map(|(id, profile)| QuotaRefreshRequest::for_profile(id, profile, cwd.clone()))
        .collect()
}

pub fn spawn_quota_refresher() -> (
    tokio::sync::watch::Sender<QuotaRefreshBatch>,
    tokio::sync::mpsc::Receiver<QuotaUpdate>,
) {
    let (profiles_tx, mut profiles_rx) = tokio::sync::watch::channel(QuotaRefreshBatch::default());
    let (updates_tx, updates_rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        let mut quotas = QuotaManager::default();
        let mut batch = QuotaRefreshBatch::default();
        let mut interval = tokio::time::interval(QUOTA_REFRESH_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            tokio::select! {
                _ = interval.tick(), if !batch.profiles.is_empty() => {
                    if !refresh_profile_quotas(
                        &mut quotas,
                        batch.generation,
                        &batch.profiles,
                        &updates_tx,
                    ).await {
                        break;
                    }
                }
                changed = profiles_rx.changed() => {
                    if changed.is_err() {
                        tracing::debug!("quota profile target feed closed; stopping quota refresher");
                        break;
                    }
                    batch = profiles_rx.borrow_and_update().clone();
                    if !refresh_profile_quotas(
                        &mut quotas,
                        batch.generation,
                        &batch.profiles,
                        &updates_tx,
                    ).await {
                        break;
                    }
                }
            }
        }
        quotas.shutdown().await;
    });
    (profiles_tx, updates_rx)
}

pub(super) async fn refresh_profile_quotas(
    quotas: &mut QuotaManager,
    generation: u64,
    profiles: &[QuotaRefreshRequest],
    updates: &tokio::sync::mpsc::Sender<QuotaUpdate>,
) -> bool {
    let ids = profiles
        .iter()
        .map(|profile| profile.profile_id.clone())
        .collect::<Vec<_>>();
    if updates
        .send(QuotaUpdate::Refreshing { profile_ids: ids })
        .await
        .is_err()
    {
        tracing::debug!("quota update consumer closed before refresh started");
        return false;
    }
    // Keep draining even if the UI is gone so codex clients return to the
    // manager for a clean shutdown; just stop sending.
    let delivered = AtomicBool::new(true);
    quotas
        .refresh_profiles(profiles.to_vec(), |quota| {
            let delivered = &delivered;
            async move {
                if delivered.load(Ordering::Acquire)
                    && updates.send(QuotaUpdate::Report(quota)).await.is_err()
                {
                    tracing::debug!("quota update consumer closed while reporting a profile");
                    delivered.store(false, Ordering::Release);
                }
            }
        })
        .await;
    if !delivered.into_inner() {
        return false;
    }
    if updates
        .send(QuotaUpdate::Finished { generation })
        .await
        .is_err()
    {
        tracing::debug!(
            generation,
            "quota update consumer closed before refresh completed"
        );
        false
    } else {
        true
    }
}

/// Keep every configured container image current, away from any session
/// launch.
///
/// `plan` is called on every tick rather than once, so a config reload changes
/// what gets refreshed without a daemon restart. Hosts refresh concurrently;
/// each host runs its own commands in order.
pub fn spawn_image_refresher(
    plan: impl Fn() -> Vec<ImageRefresh> + Send + 'static,
    cancellation: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + IMAGE_REFRESH_DELAY,
            IMAGE_REFRESH_INTERVAL,
        );
        // A refresh slower than the interval collapses the ticks it missed
        // instead of stacking a second pull behind the first.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                // Quitting wins over a tick that came due during a long
                // refresh, so shutdown never starts one more pull.
                biased;
                _ = cancellation.cancelled() => return,
                _ = interval.tick() => refresh_images(plan(), &cancellation).await,
            }
        }
    })
}

pub(super) async fn refresh_images(
    plan: Vec<ImageRefresh>,
    cancellation: &tokio_util::sync::CancellationToken,
) {
    if plan.is_empty() {
        return;
    }
    // One flag for every host, so quitting kills the pulls in flight instead of
    // waiting out a multi-gigabyte download.
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut hosts = tokio::task::JoinSet::new();
    for refresh in plan {
        // ProcessExecutor is synchronous, and a pull is long: it belongs on a
        // blocking thread, never on the runtime.
        let executor = CancellableProcessExecutor::new(cancelled.clone());
        hosts.spawn_blocking(move || {
            let Err(error) = refresh_host_image(&refresh, &executor) else {
                return;
            };
            if executor.is_cancelled() {
                // The daemon is leaving. That is not a fault of the host.
                tracing::debug!(
                    host = refresh.host.label(),
                    image = refresh.image,
                    "container image refresh cancelled"
                );
                return;
            }
            tracing::warn!(
                host = refresh.host.label(),
                image = refresh.image,
                error = format!("{error:#}"),
                "could not refresh a container image"
            );
        });
    }
    let mut cancelling = false;
    loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled(), if !cancelling => {
                cancelling = true;
                cancelled.store(true, Ordering::Release);
            }
            joined = hosts.join_next() => match joined {
                None => return,
                Some(Ok(())) => {}
                Some(Err(error)) => {
                    tracing::warn!(%error, "container image refresh task failed");
                }
            },
        }
    }
}

/// Pull one image on one host, then drop whatever that unlinked.
///
/// The image id before and after says whether the pull actually changed
/// anything, which is the only part worth an `info` line.
pub(super) fn refresh_host_image(
    refresh: &ImageRefresh,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let cached = image_id(&refresh.image_id, executor);
    run_refresh_command(&refresh.pull, executor)?;
    let pulled = image_id(&refresh.image_id, executor);
    if pulled.is_some() && pulled != cached {
        tracing::info!(
            host = refresh.host.label(),
            image = refresh.image,
            id = pulled.unwrap_or_default(),
            "pulled a newer container image"
        );
    } else {
        tracing::debug!(
            host = refresh.host.label(),
            image = refresh.image,
            "container image is already current"
        );
    }
    run_refresh_command(&refresh.prune, executor)?;
    Ok(())
}

/// The host's id for an image, or `None` when it has no copy of it yet. A
/// missing image is the ordinary first-pull case, not a fault.
pub(super) fn image_id(command: &CommandSpec, executor: &impl CommandExecutor) -> Option<String> {
    let output = executor.execute(command).ok()?;
    if output.status != 0 {
        return None;
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!id.is_empty()).then_some(id)
}

pub(super) fn run_refresh_command(
    command: &CommandSpec,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let output = executor.execute(command)?;
    if output.status != 0 {
        bail!(
            "{} failed with status {}: {}",
            command.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub fn complete_manual_quota_refresh(
    pending_generation: &mut Option<u64>,
    completed_generation: u64,
) -> bool {
    if *pending_generation != Some(completed_generation) {
        return false;
    }
    *pending_generation = None;
    true
}

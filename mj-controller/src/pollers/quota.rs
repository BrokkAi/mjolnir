use super::*;

pub fn quota_refresh_profiles(controller: &Controller) -> Vec<QuotaRefreshRequest> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let runtime = crate::quota::QuotaRuntimeConfig::of(&controller.config);
    controller
        .config
        .enabled_profiles()
        .map(|(id, profile)| {
            QuotaRefreshRequest::for_profile(id, profile, cwd.clone(), runtime.clone())
        })
        .collect()
}

/// Reads a profile's stored report, the rebuildable copy the daemon keeps in
/// the store. Called on a blocking thread.
pub type QuotaCacheLoader =
    Arc<dyn Fn(&QuotaRefreshRequest) -> Option<crate::quota::ProfileQuota> + Send + Sync>;

/// When a profile is next due for a probe, in epoch seconds: one interval
/// after its last report, or the end of a rate-limit hold when that is later.
/// A profile with no report is due at once (zero).
pub(super) fn next_probe_at(report: Option<&crate::quota::ProfileQuota>) -> u64 {
    report.map_or(0, |report| {
        (report.refreshed_at_epoch_seconds + QUOTA_REFRESH_INTERVAL.as_secs())
            .max(report.rate_limited_until_epoch_seconds.unwrap_or(0))
    })
}

/// Whether a provider told this profile to wait, and the wait is not over.
fn on_hold(report: Option<&crate::quota::ProfileQuota>, now: u64) -> bool {
    report
        .and_then(|report| report.rate_limited_until_epoch_seconds)
        .is_some_and(|until| until > now)
}

/// The daemon's quota poller, the only process that asks a provider.
///
/// Each profile has its own schedule. A profile whose stored report is younger
/// than [`QUOTA_REFRESH_INTERVAL`] is published as it is and probed when that
/// report ages out; anything else is probed now. A batch with `refresh` set
/// probes every profile.
pub fn spawn_quota_refresher(
    cache: QuotaCacheLoader,
) -> (
    tokio::sync::watch::Sender<QuotaRefreshBatch>,
    tokio::sync::mpsc::Receiver<QuotaUpdate>,
) {
    let (profiles_tx, mut profiles_rx) = tokio::sync::watch::channel(QuotaRefreshBatch::default());
    let (updates_tx, updates_rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        // A daemon that stopped mid-check left its quota container running;
        // this instance owns it, and nothing else will remove it. Unit tests
        // start pollers too, with the instance identity of the user's real
        // daemon, and must never reach the host's container engine.
        if !cfg!(test)
            && let Err(error) = tokio::task::spawn_blocking(|| {
                crate::quota_runtime::sweep_quota_containers(
                    &mj_core::targets::BoundedProcessExecutor::new(Duration::from_secs(30)),
                    std::env::var_os("PATH").as_deref(),
                )
            })
            .await
        {
            tracing::warn!(%error, "quota container sweep task failed");
        }
        let mut quotas = QuotaManager::default();
        let mut batch = QuotaRefreshBatch::default();
        // The cache identity each profile was last adopted under. A changed
        // identity is a changed profile, whose report no longer applies.
        let mut identities: std::collections::BTreeMap<String, String> = Default::default();
        loop {
            let now = epoch_seconds();
            let wake = batch
                .profiles
                .iter()
                .map(|request| next_probe_at(quotas.report(&request.profile_id)))
                .min()
                .map(|due| Duration::from_secs(due.saturating_sub(now).max(5)));
            tokio::select! {
                _ = tokio::time::sleep(wake.unwrap_or_default()), if wake.is_some() => {}
                changed = profiles_rx.changed() => {
                    if changed.is_err() {
                        tracing::debug!("quota profile target feed closed; stopping quota refresher");
                        break;
                    }
                    batch = profiles_rx.borrow_and_update().clone();
                    if !adopt_profiles(&mut quotas, &mut identities, &batch, &cache, &updates_tx).await {
                        break;
                    }
                }
            }
            let now = epoch_seconds();
            let due = batch
                .profiles
                .iter()
                .filter(|request| {
                    let report = quotas.report(&request.profile_id);
                    // A hold outranks even an explicit refresh: probing an
                    // endpoint that just said 429 only extends the limit.
                    !on_hold(report, now) && (batch.refresh || next_probe_at(report) <= now)
                })
                .cloned()
                .collect::<Vec<_>>();
            // A batch that only changed the profile set and finds nothing due
            // is not a cycle. A requested refresh always is, so the person's
            // notice can end.
            if due.is_empty() && !batch.refresh {
                continue;
            }
            let generation = batch.generation;
            batch.refresh = false;
            if !refresh_profile_quotas(&mut quotas, generation, &due, &updates_tx).await {
                break;
            }
        }
        quotas.shutdown().await;
    });
    (profiles_tx, updates_rx)
}

/// Bring the manager in line with a new batch: drop profiles that left or
/// changed, and adopt the stored report of each new one when it is still
/// current. Reports whether the consumer is still listening.
async fn adopt_profiles(
    quotas: &mut QuotaManager,
    identities: &mut std::collections::BTreeMap<String, String>,
    batch: &QuotaRefreshBatch,
    cache: &QuotaCacheLoader,
    updates: &tokio::sync::mpsc::Sender<QuotaUpdate>,
) -> bool {
    let keep = batch
        .profiles
        .iter()
        .map(|request| request.profile_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    identities.retain(|id, _| keep.contains(id));
    quotas.retain_profiles(&keep).await;
    for request in &batch.profiles {
        let identity = request.cache_identity();
        if identities.get(&request.profile_id) == Some(&identity) {
            continue;
        }
        identities.insert(request.profile_id.clone(), identity);
        quotas.forget(&request.profile_id).await;
        let load = cache.clone();
        let for_request = request.clone();
        let stored = match tokio::task::spawn_blocking(move || load(&for_request)).await {
            Ok(stored) => stored,
            Err(error) => {
                tracing::warn!(%error, "stored quota read task failed");
                None
            }
        };
        let Some(stored) = stored.filter(|report| {
            report.error.is_none() && next_probe_at(Some(report)) > epoch_seconds()
        }) else {
            continue;
        };
        tracing::debug!(
            profile_id = %request.profile_id,
            refreshed_at = stored.refreshed_at_epoch_seconds,
            "using the stored quota report; the next probe waits for it to age"
        );
        quotas.seed(stored.clone());
        let outcome = QuotaRefreshOutcome {
            report: stored,
            credentials_changed: false,
        };
        if updates.send(QuotaUpdate::Report(outcome)).await.is_err() {
            return false;
        }
    }
    true
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
        .probe(profiles.to_vec(), |quota| {
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

/// What the daemon wants to tell the user about a background image download.
///
/// The refresher logs every detail; these are the few moments worth a notice
/// in the dashboard, because the person's first session waits on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageRefreshReport {
    /// A download has just begun.
    Started { host: String, image: String },
    /// A download finished and left the host with the image.
    Pulled { host: String, image: String },
    /// A download failed. Reported once per distinct error, not once an hour.
    Failed {
        host: String,
        image: String,
        error: String,
    },
}

/// Download every configured container image the host lacks, and keep the
/// ones that track a moving tag current, away from any session launch.
///
/// The first pass runs shortly after the daemon starts, which is what spares
/// the person's first session a multi-gigabyte download. `plan` is called on
/// every tick rather than once, so a config reload changes what gets
/// downloaded without a daemon restart. Hosts refresh concurrently; each host
/// runs its own commands in order.
///
/// `report` is how the daemon speaks: it is called from the blocking download
/// threads as well as from this task, so it must be cheap and must not block.
pub fn spawn_image_refresher(
    plan: impl Fn() -> Vec<ImageRefresh> + Send + 'static,
    report: impl Fn(ImageRefreshReport) + Send + Sync + 'static,
    apple_container_settled: tokio_util::sync::CancellationToken,
    cancellation: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let report: Arc<dyn Fn(ImageRefreshReport) + Send + Sync> = Arc::new(report);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + IMAGE_REFRESH_DELAY,
            IMAGE_REFRESH_INTERVAL,
        );
        // A refresh slower than the interval collapses the ticks it missed
        // instead of stacking a second pull behind the first.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The last error reported for each host and image. An unreachable
        // host must not produce the same notice every hour.
        let mut last_failures: BTreeMap<String, String> = BTreeMap::new();
        loop {
            tokio::select! {
                // Quitting wins over a tick that came due during a long
                // refresh, so shutdown never starts one more pull.
                biased;
                _ = cancellation.cancelled() => return,
                _ = interval.tick() => {
                    refresh_images(
                        plan(),
                        &report,
                        &mut last_failures,
                        &apple_container_settled,
                        &cancellation,
                    )
                    .await;
                }
            }
        }
    })
}

/// The key that identifies one host's copy of one image, for failure
/// suppression.
fn refresh_key(host: &str, image: &str) -> String {
    format!("{host}|{image}")
}

/// Report a failed download once, and stay quiet while it keeps failing the
/// same way.
///
/// A host that is simply offline fails identically every hour, and a notice
/// an hour would be noise. A different error is new information, and so is a
/// failure after a success, which is why success clears the record.
pub(super) fn record_refresh_result(
    last_failures: &mut BTreeMap<String, String>,
    host: &str,
    image: &str,
    error: Option<String>,
    report: &dyn Fn(ImageRefreshReport),
) {
    let key = refresh_key(host, image);
    let Some(error) = error else {
        last_failures.remove(&key);
        return;
    };
    if last_failures.get(&key) == Some(&error) {
        return;
    }
    last_failures.insert(key, error.clone());
    report(ImageRefreshReport::Failed {
        host: host.to_owned(),
        image: image.to_owned(),
        error,
    });
}

/// Whether a local image host's engine can be run at all.
///
/// Only local hosts are checked: a remote host's engine lives on the other
/// side of ssh, and a failure there is real news about that host.
pub(super) fn local_engine_installed(host: &ImageHost, path: Option<&std::ffi::OsStr>) -> bool {
    match host {
        ImageHost::LocalPodman | ImageHost::LocalDocker | ImageHost::AppleContainer => {
            crate::targets::program_on_path(host.engine(), path)
        }
        ImageHost::SshPodman(_) | ImageHost::SshDocker(_) | ImageHost::SshAppleContainer(_) => true,
    }
}

/// Whether a host's engine is up, so a background refresh may use it.
///
/// A refresh is a chore, not a launch: it only observes Apple's service and
/// never starts one the user stopped. The next launch preflight starts it
/// when a session needs it.
pub(super) fn host_is_running(host: &ImageHost, executor: &impl CommandExecutor) -> Result<bool> {
    match host {
        ImageHost::AppleContainer => Ok(matches!(
            crate::targets::apple_container_runtime_status(executor)?,
            crate::targets::AppleContainerStatus::Running
        )),
        _ => Ok(true),
    }
}

/// One host's turn in a refresh tick. Answers `None` when there is nothing to
/// record: the host is stopped, or the daemon is leaving.
pub(super) fn refresh_one_host(
    refresh: ImageRefresh,
    executor: &impl CommandExecutor,
    report: &dyn Fn(ImageRefreshReport),
) -> Option<(String, String, Option<String>)> {
    let host = refresh.host.label();
    // `None` means the host's engine is stopped: nothing to refresh.
    let outcome = host_is_running(&refresh.host, executor).and_then(|running| {
        if !running {
            return Ok(None);
        }
        refresh_host_image(&refresh, executor, report).map(Some)
    });
    let error = match outcome {
        Ok(Some(_)) => None,
        Ok(None) => {
            tracing::debug!(
                host,
                image = refresh.image,
                "the container engine is not running; skipping the image refresh"
            );
            return None;
        }
        Err(error) if executor.cancellation_requested() => {
            // The daemon is leaving. That is not a fault of the host,
            // and it is not news for the user either.
            tracing::debug!(
                host,
                image = refresh.image,
                error = format!("{error:#}"),
                "container image refresh cancelled"
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(
                host,
                image = refresh.image,
                error = format!("{error:#}"),
                "could not refresh a container image"
            );
            Some(format!("{error:#}"))
        }
    };
    Some((host, refresh.image, error))
}

pub(super) async fn refresh_images(
    plan: Vec<ImageRefresh>,
    report: &Arc<dyn Fn(ImageRefreshReport) + Send + Sync>,
    last_failures: &mut BTreeMap<String, String>,
    apple_container_settled: &tokio_util::sync::CancellationToken,
    cancellation: &tokio_util::sync::CancellationToken,
) {
    if plan.is_empty() {
        return;
    }
    // Pulls are restartable preparation. Shutdown cancels their subprocesses;
    // the next daemon discovers any image still missing or out of date.
    // One flag for every host, so quitting kills the pulls in flight instead of
    // waiting out a multi-gigabyte download.
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut hosts = tokio::task::JoinSet::new();
    for refresh in plan {
        // The default configuration names a podman, a docker, and on macOS an
        // Apple container target whether or not the engine is installed. An
        // engine that is not on this machine is not a failed download, and it
        // must not become a notice on every start.
        if !local_engine_installed(&refresh.host, std::env::var_os("PATH").as_deref()) {
            tracing::debug!(
                host = refresh.host.label(),
                image = refresh.image,
                "container engine is not installed; skipping the image refresh"
            );
            continue;
        }
        let executor = CancellableProcessExecutor::new(cancelled.clone());
        let report = report.clone();
        hosts.spawn(refresh_host_after_startup(
            refresh,
            apple_container_settled.clone(),
            cancellation.clone(),
            move |refresh| refresh_one_host(refresh, &executor, &*report),
        ));
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
                Some(Ok(None)) => {}
                Some(Ok(Some((host, image, error)))) => {
                    record_refresh_result(last_failures, &host, &image, error, &**report);
                }
                Some(Err(error)) => {
                    tracing::warn!(%error, "container image refresh task failed");
                }
            },
        }
    }
}

/// One host's refresh task. The local Apple container refresh first waits
/// for daemon startup's start of the service to finish: the refresh only looks
/// at the service and never starts it, and a look mid-start would read
/// "stopped" and skip the whole interval. Every other host starts at once, so
/// a slow Apple start holds no other host's refresh.
pub(super) async fn refresh_host_after_startup(
    refresh: ImageRefresh,
    apple_container_settled: tokio_util::sync::CancellationToken,
    cancellation: tokio_util::sync::CancellationToken,
    run: impl FnOnce(ImageRefresh) -> Option<(String, String, Option<String>)> + Send + 'static,
) -> Option<(String, String, Option<String>)> {
    if matches!(refresh.host, ImageHost::AppleContainer) {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return None,
            _ = apple_container_settled.cancelled() => {}
        }
    }
    // ProcessExecutor is synchronous, and a pull is long: it belongs on a
    // blocking thread, never on the runtime.
    match tokio::task::spawn_blocking(move || run(refresh)).await {
        Ok(result) => result,
        Err(error) => {
            tracing::warn!(%error, "container image refresh task failed");
            None
        }
    }
}

/// What one host's refresh of one image did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageRefreshOutcome {
    /// The host already had the image and this target only wants it present,
    /// so nothing was downloaded.
    Present,
    /// A pull ran and the host's copy did not change.
    Unchanged,
    /// A pull ran and left the host with a different image.
    Pulled { id: String },
}

/// Pull one image on one host, then drop whatever that unlinked.
///
/// The image id before and after says whether the pull actually changed
/// anything, which is the only part worth an `info` line. An image that is
/// only downloaded when absent skips the pull, and the prune with it, as soon
/// as the host reports a copy.
///
/// `report` is told when a download actually starts and when one leaves the
/// host with a new image, so the user hears about the wait they are in rather
/// than about every hourly check. The same gate covers background refreshes,
/// explicit preloads, and session launches, including other mj processes.
pub fn refresh_host_image(
    refresh: &ImageRefresh,
    executor: &impl CommandExecutor,
    report: &dyn Fn(ImageRefreshReport),
) -> Result<ImageRefreshOutcome> {
    let lock = crate::image_pull_gate::image_pull_mutex(&refresh.host, &refresh.image);
    let _guard = crate::image_pull_gate::hold_image_pull(
        &lock,
        || executor.cancellation_requested(),
        || {},
    )?;
    let host = refresh.host.label();
    let cached = image_id(&refresh.image_id, executor);
    if refresh.when == RefreshWhen::WhenAbsent && cached.is_some() {
        tracing::debug!(
            host,
            image = refresh.image,
            "the host already has this container image"
        );
        return Ok(ImageRefreshOutcome::Present);
    }
    // Only a host with no copy is about to download for real. An hourly
    // refresh of a moving tag usually finds nothing newer, and announcing it
    // every hour would be noise.
    if cached.is_none() {
        report(ImageRefreshReport::Started {
            host: host.clone(),
            image: refresh.image.clone(),
        });
    }
    run_refresh_command(&refresh.pull, executor)?;
    let pulled = image_id(&refresh.image_id, executor);
    let outcome = if pulled.is_some() && (cached.is_none() || pulled != cached) {
        let id = pulled.unwrap_or_default();
        tracing::info!(
            host,
            image = refresh.image,
            id,
            "pulled a newer container image"
        );
        report(ImageRefreshReport::Pulled {
            host,
            image: refresh.image.clone(),
        });
        ImageRefreshOutcome::Pulled { id }
    } else {
        tracing::debug!(
            host,
            image = refresh.image,
            "container image is already current"
        );
        ImageRefreshOutcome::Unchanged
    };
    run_refresh_command(&refresh.prune, executor)?;
    Ok(outcome)
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

/// Whether a refresh the person asked for is over. `pending_cycles` is the
/// daemon's finished-cycle count at the moment of the request, and
/// `finished_cycles` is its count now. A cycle already running when the
/// request arrived can end first and complete the notice a few seconds early;
/// the report itself is what the row shows, so that is harmless.
pub fn complete_manual_quota_refresh(
    pending_cycles: &mut Option<u64>,
    finished_cycles: u64,
) -> bool {
    if !pending_cycles.is_some_and(|pending| finished_cycles > pending) {
        return false;
    }
    *pending_cycles = None;
    true
}

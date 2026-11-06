use super::*;

pub fn spawn_dashboard_capacity_poller() -> (
    tokio::sync::watch::Sender<Vec<DeploymentCapacityTarget>>,
    tokio::sync::mpsc::Sender<()>,
    tokio::sync::mpsc::Receiver<CapacityPollUpdate>,
) {
    spawn_capacity_poller_with(sample_capacity_target)
}

/// Sample one deployment host now. The periodic poller and launch admission
/// share this path so both apply the same timeout and local/remote rules.
pub(crate) async fn sample_capacity_target(
    target: DeploymentCapacityTarget,
) -> Result<Option<DeploymentCapacityUsage>> {
    if let Some(error) = &target.probe_error {
        bail!("capacity probe is unavailable: {error}");
    }
    if target.local {
        let mut usage = collect_local_capacity_with(collect_local_capacity).await?;
        usage.storage = collect_local_storage(&target).await;
        return Ok(Some(usage));
    }
    tokio::time::timeout(RESOURCE_POLL_TIMEOUT, collect_capacity(&target))
        .await
        .context("capacity probe timed out")?
}

pub(super) fn spawn_capacity_poller_with<F, Fut>(
    collect: F,
) -> (
    tokio::sync::watch::Sender<Vec<DeploymentCapacityTarget>>,
    tokio::sync::mpsc::Sender<()>,
    tokio::sync::mpsc::Receiver<CapacityPollUpdate>,
)
where
    F: Fn(DeploymentCapacityTarget) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Option<DeploymentCapacityUsage>>> + Send + 'static,
{
    let (targets_tx, mut targets_rx) =
        tokio::sync::watch::channel(Vec::<DeploymentCapacityTarget>::new());
    let (updates_tx, updates_rx) = tokio::sync::mpsc::channel(64);
    let (triggers_tx, mut triggers_rx) = tokio::sync::mpsc::channel(1);
    tokio::spawn(async move {
        let mut targets = Vec::new();
        let collect = Arc::new(collect);
        let mut samples = CapacitySamples::default();
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + CAPACITY_POLL_INTERVAL,
            CAPACITY_POLL_INTERVAL,
        );
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = updates_tx.closed() => break,
                _ = interval.tick() => {
                    samples.schedule(targets.iter().cloned(), &collect);
                }
                changed = targets_rx.changed() => {
                    if changed.is_err() {
                        tracing::debug!("capacity poll target feed closed; stopping capacity poller");
                        break;
                    }
                    let updated = targets_rx.borrow_and_update().clone();
                    samples.schedule(
                        updated.iter().filter(|target| !targets.contains(target)).cloned(),
                        &collect,
                    );
                    targets = updated;
                }
                trigger = triggers_rx.recv() => {
                    if trigger.is_none() {
                        break;
                    }
                    samples.schedule(targets.iter().cloned(), &collect);
                }
                completed = samples.tasks.join_next_with_id(), if !samples.tasks.is_empty() => {
                    let (id, result) = match completed.expect("capacity task exists") {
                        Ok((id, result)) => (id, result.map_err(|error| format!("{error:#}"))),
                        Err(error) => (error.id(), Err(format!("capacity probe task failed: {error}"))),
                    };
                    let sampled = samples.targets.remove(&id).expect("capacity task retains its target");
                    if let Err(error) = &result {
                        tracing::warn!(target_id = %sampled.id, %error, "capacity probe failed");
                    }
                    let Ok(permit) = updates_tx.reserve().await else {
                        break;
                    };
                    // A watch update and completion can become ready together.
                    // Revalidate after backpressure, with no await between
                    // reading the latest target and publishing the result.
                    let current = targets_rx.borrow().iter().find(|target| target.id == sampled.id).cloned();
                    let Some(current) = current else {
                        continue;
                    };
                    if current != sampled {
                        // A changed target gets one follow-up; its old result
                        // must not overwrite a reading for the new configuration.
                        // If changed() is still pending, that arm will start it.
                        if targets.contains(&current) {
                            samples.schedule(std::iter::once(current), &collect);
                        }
                        continue;
                    }
                    permit.send(CapacityPollUpdate {
                        target_id: sampled.id,
                        result,
                        sampled_at_epoch_seconds: epoch_seconds(),
                    });
                }
            }
        }
        samples.tasks.abort_all();
        while let Some(completed) = samples.tasks.join_next().await {
            match completed {
                Ok(Err(error)) => tracing::warn!(%error, "capacity probe failed during shutdown"),
                Err(error) if !error.is_cancelled() => {
                    tracing::error!(%error, "capacity probe task failed during shutdown");
                }
                _ => {}
            }
        }
    });
    (targets_tx, triggers_tx, updates_rx)
}

#[derive(Default)]
pub(super) struct CapacitySamples {
    pub(super) tasks: tokio::task::JoinSet<Result<Option<DeploymentCapacityUsage>>>,
    pub(super) targets: std::collections::HashMap<tokio::task::Id, DeploymentCapacityTarget>,
}

impl CapacitySamples {
    pub(super) fn schedule<F, Fut>(
        &mut self,
        targets: impl IntoIterator<Item = DeploymentCapacityTarget>,
        collect: &Arc<F>,
    ) where
        F: Fn(DeploymentCapacityTarget) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<DeploymentCapacityUsage>>> + Send + 'static,
    {
        for target in targets {
            if self.targets.values().any(|running| running.id == target.id) {
                continue;
            }
            let collect = collect.clone();
            let sampled = target.clone();
            let task = self.tasks.spawn(async move {
                let started = Instant::now();
                let target_id = sampled.id.clone();
                let result = collect(sampled).await;
                tracing::debug!(
                    %target_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    success = result.is_ok(),
                    "capacity probe completed",
                );
                result
            });
            self.targets.insert(task.id(), target);
        }
    }
}

pub(crate) async fn collect_capacity(
    target: &DeploymentCapacityTarget,
) -> Result<Option<DeploymentCapacityUsage>> {
    if let Some(error) = &target.probe_error {
        anyhow::bail!("capacity probe is unavailable: {error}");
    }
    match target.kind {
        DeploymentCapacityKind::Host => {
            let mut last_error = None;
            for command in &target.probes {
                match execute_resource_command(command).await {
                    Ok(output) => {
                        return crate::targets::parse_host_capacity(
                            &output.stdout,
                            &probe_storage_host(command),
                        )
                        .map(Some);
                    }
                    Err(error) => last_error = Some(error),
                }
            }
            Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no host probe is configured")))
        }
        DeploymentCapacityKind::AwsFleet => {
            if target.probes.is_empty() {
                return Ok(None);
            }
            let mut tasks = tokio::task::JoinSet::new();
            for command in target.probes.clone() {
                tasks.spawn(async move {
                    let output = execute_resource_command(&command).await?;
                    crate::targets::parse_aws_allocated_capacity(
                        &output.stdout,
                        &probe_storage_host(&command),
                    )
                });
            }
            let mut usages = Vec::new();
            while let Some(result) = tasks.join_next().await {
                usages.push(result.context("join EC2 capacity probe")??);
            }
            aggregate_aws_capacity(&usages).map(Some)
        }
    }
}

pub fn aggregate_aws_capacity(
    usages: &[DeploymentCapacityUsage],
) -> Result<DeploymentCapacityUsage> {
    let mut total = DeploymentCapacityUsage {
        cpu_percent: None,
        memory_used_bytes: 0,
        memory_total_bytes: 0,
        logical_cores: 0,
        disk_total_bytes: Some(0),
        storage: Vec::new(),
    };
    for usage in usages {
        total.storage.extend(usage.storage.iter().cloned());
        total.memory_total_bytes = total
            .memory_total_bytes
            .checked_add(usage.memory_total_bytes)
            .context("aggregate EC2 RAM overflow")?;
        total.logical_cores = total
            .logical_cores
            .checked_add(usage.logical_cores)
            .context("aggregate EC2 core count overflow")?;
        total.disk_total_bytes = Some(
            total
                .disk_total_bytes
                .unwrap_or(0)
                .checked_add(usage.disk_total_bytes.unwrap_or(0))
                .context("aggregate EC2 disk overflow")?,
        );
    }
    Ok(total)
}

pub(super) fn collect_local_capacity() -> Result<DeploymentCapacityUsage> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    // Frequency is unused and scans every core in parallel on each refresh.
    system.refresh_cpu_usage();
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    system.refresh_cpu_usage();
    Ok(DeploymentCapacityUsage {
        cpu_percent: Some(system.global_cpu_usage().round().clamp(0.0, 100.0) as u8),
        memory_used_bytes: system
            .total_memory()
            .saturating_sub(system.available_memory()),
        memory_total_bytes: system.total_memory(),
        logical_cores: system
            .cpus()
            .len()
            .try_into()
            .context("logical CPU count overflow")?,
        disk_total_bytes: None,
        storage: Vec::new(),
    })
}

/// The storage owner's name for the machine a probe ran on.
fn probe_storage_host(command: &CommandSpec) -> String {
    mj_core::targets::storage::storage_host_of_destination(command.ssh_destination.as_deref())
}

/// Measure local free space over the local target's storage paths. A failed
/// measurement leaves storage unknown; CPU and memory still publish.
async fn collect_local_storage(
    target: &DeploymentCapacityTarget,
) -> Vec<mj_core::targets::storage::HostStorageSample> {
    let paths = target.local_storage_paths.clone();
    let measured = tokio::task::spawn_blocking(move || {
        crate::targets::measure_local_storage(
            &paths,
            &crate::targets::BoundedProcessExecutor::new(RESOURCE_POLL_TIMEOUT),
        )
    })
    .await
    .context("join the local free-space probe")
    .and_then(|measured| measured);
    match measured {
        Ok((_, filesystems)) if filesystems.is_empty() => Vec::new(),
        Ok((home, filesystems)) => vec![mj_core::targets::storage::HostStorageSample {
            host: mj_core::targets::storage::LOCAL_STORAGE_HOST.to_owned(),
            home,
            filesystems,
        }],
        Err(error) => {
            tracing::warn!(
                error = format!("{error:#}"),
                "local free-space probe failed"
            );
            Vec::new()
        }
    }
}

pub(crate) async fn collect_local_capacity_with(
    collect: impl FnOnce() -> Result<DeploymentCapacityUsage> + Send + 'static,
) -> Result<DeploymentCapacityUsage> {
    // A blocking sample cannot be cancelled. Keep its slot occupied
    // until it exits, even when the deadline has elapsed.
    let mut sample = tokio::task::spawn_blocking(move || {
        let result = collect();
        // Shutdown can drop the awaiting future before this thread exits.
        if let Err(error) = &result {
            tracing::warn!(%error, "local capacity sample failed");
        }
        result
    });
    match tokio::time::timeout(RESOURCE_POLL_TIMEOUT, &mut sample).await {
        Ok(result) => result.context("join local capacity probe")?,
        Err(_) => {
            match sample.await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::warn!(%error, "timed-out capacity probe failed"),
                Err(error) => tracing::error!(%error, "timed-out capacity probe task failed"),
            }
            bail!("capacity probe timed out")
        }
    }
}

/// Run one resource probe. A probe the SSH server turned away never ran, so
/// it is retried with the same backoff the executor and the relay use (launch
/// finding R3-5); any other failure is reported at once.
pub(super) async fn execute_resource_command(command: &CommandSpec) -> Result<CommandOutput> {
    use crate::targets::{SshRefusalRetry, ssh_refusal};
    // The poller bounds the whole probe, retries included, with
    // `RESOURCE_POLL_TIMEOUT`; a refusal that outlasts the retry bound fails it sooner.
    let mut retry = SshRefusalRetry::for_command(command);
    loop {
        let (output, ssh_session) = run_resource_command(command, retry.walk()).await?;
        if output.status == 0 {
            return Ok(output);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        // A probe that printed the start marker ran, so its status is its own.
        let refusal = match &ssh_session {
            Some(lease) => lease.gate().refusal(output.status, &stderr),
            None => ssh_refusal(output.status, &stderr),
        };
        if let Some(destination) = command.ssh_destination.as_deref()
            && let Some(refusal) = refusal
        {
            // The lease records what the refusal means while it is still held.
            let walks_on = ssh_session
                .as_ref()
                .is_some_and(|lease| lease.note_refusal(refusal));
            // Only Unix leases own a shared session slot; free it before the wait.
            #[cfg(unix)]
            drop(ssh_session);
            let delay = retry
                .next(refusal, walks_on, destination, &command.purpose, &stderr)
                .with_context(|| format!("{} failed", command.purpose))?;
            tokio::time::sleep(delay).await;
            continue;
        }
        bail!(
            "{} failed with status {}: {}",
            command.purpose,
            output.status,
            stderr.trim()
        );
    }
}

/// Spawn one probe and collect its output, with the SSH session lease it ran
/// on. The lease is returned so it outlives the child.
async fn run_resource_command(
    command: &CommandSpec,
    walk: &mut crate::targets::SshWalk,
) -> Result<(CommandOutput, Option<crate::targets::SshSessionLease>)> {
    // A remote probe the daemon runs itself is one session on a shared SSH
    // connection; the lease is held until the probe has exited. A probe that
    // runs on the node is leased, retried and stripped of the start marker
    // there, beside the connection it shares.
    let (command, ssh_session) =
        if command.ssh_session.is_some() && crate::targets::command_host(command).is_none() {
            let requested = command.clone();
            let executor = crate::targets::CancellableProcessExecutor::with_timeout(
                crate::targets::SSH_MASTER_OPEN_TIMEOUT,
            );
            let _cancel_preparation = executor.cancel_on_drop();
            let mut walked = std::mem::take(walk);
            let (prepared, walked) = tokio::task::spawn_blocking(move || {
                let prepared = requested
                    .open_ssh_session(&executor, &mut walked)
                    .map(crate::targets::SessionCommand::into_parts);
                (prepared, walked)
            })
            .await
            .context("join the SSH session lease for a resource probe")?;
            *walk = walked;
            prepared?
        } else {
            (command.clone(), None)
        };
    let command = &command;
    let crate::streamed_child::ProxyStdio {
        mut child,
        stdin,
        stdout,
        stderr,
    } = crate::streamed_child::spawn_session_proxy(command, ssh_session.as_ref())
        .await
        .with_context(|| format!("start {}", command.purpose))?;
    // Resource probes do not read input; closing the writer gives a child
    // that waits for EOF the signal it expects.
    drop(stdin);
    let output = tokio::time::timeout(RESOURCE_POLL_TIMEOUT, async {
        let (stdout, stderr) = tokio::try_join!(
            crate::streamed_child::collect_bounded(stdout, 8 * 1024 * 1024),
            crate::streamed_child::collect_bounded(stderr, 8 * 1024 * 1024),
        )?;
        let exit = child.wait().await?;
        Ok::<_, std::io::Error>((stdout, stderr, exit))
    })
    .await
    .with_context(|| format!("wait for {}", command.purpose))?
    .with_context(|| format!("read {}", command.purpose))?;
    let command_output = CommandOutput {
        status: output.2.code().unwrap_or(-1),
        stdout: output.0,
        stderr: output.1,
    };
    Ok((command_output, ssh_session))
}

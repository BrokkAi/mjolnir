use super::*;

pub fn spawn_dashboard_capacity_poller() -> (
    tokio::sync::watch::Sender<Vec<DeploymentCapacityTarget>>,
    tokio::sync::mpsc::Sender<()>,
    tokio::sync::mpsc::Receiver<CapacityPollUpdate>,
) {
    spawn_capacity_poller_with(|target| async move {
        if let Some(error) = &target.probe_error {
            bail!("capacity probe is unavailable: {error}");
        }
        if target.local {
            return collect_local_capacity_with(collect_local_capacity)
                .await
                .map(Some);
        }
        tokio::time::timeout(RESOURCE_POLL_TIMEOUT, collect_capacity(&target))
            .await
            .context("capacity probe timed out")?
    })
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

pub(super) async fn collect_capacity(
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
                        return crate::targets::parse_host_capacity(&output.stdout).map(Some);
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
                    crate::targets::parse_aws_allocated_capacity(&output.stdout)
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
    };
    for usage in usages {
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
    })
}

pub(super) async fn collect_local_capacity_with(
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

pub(super) async fn execute_resource_command(command: &CommandSpec) -> Result<CommandOutput> {
    let mut process = tokio::process::Command::new(&command.program);
    process
        .args(&command.args)
        .envs(&command.env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = process
        .spawn()
        .with_context(|| format!("start {} for {}", command.program, command.purpose))?;
    // stdin is null; nothing writes while output drains, so this cannot hit
    // the write-then-wait deadlock the disallowed_methods lint guards against.
    #[allow(clippy::disallowed_methods)]
    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("wait for {}", command.purpose))?;
    let command_output = CommandOutput {
        status: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: output.stderr,
    };
    if command_output.status != 0 {
        bail!(
            "{} failed with status {}: {}",
            command.purpose,
            command_output.status,
            String::from_utf8_lossy(&command_output.stderr).trim()
        );
    }
    Ok(command_output)
}

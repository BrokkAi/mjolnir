use super::*;

/// An idle workspace operation holds the managed connection and a worker
/// barrier. Dropping this value disconnects and cancels the barrier; releasing
/// it resumes dispatch without claiming that an archive covers the journal.
pub struct IdleWorkspaceLease {
    pub(super) lease: ManagedSessionLease,
    pub(super) command_id: String,
    pub(super) harness: HarnessKind,
    ready_cursor: RelayCursor,
    _worker_owner: crate::worker_lifecycle::WorkerPermit,
}

impl IdleWorkspaceLease {
    /// Reserve an idle worker without taking a busy turn's control channel.
    pub async fn acquire_for_upgrade(
        handle: &ManagedSessionHandle,
        harness: HarnessKind,
    ) -> Result<Option<Self>> {
        let Some(worker_owner) = crate::worker_lifecycle::WorkerPermit::try_acquire(
            handle.session_id(),
            "idle worker reservation",
        )?
        else {
            return Ok(None);
        };
        tokio::time::timeout(Duration::from_secs(30), async {
            let Some(mut lease) = handle.lease_idle_connection(harness).await? else {
                return Ok(None);
            };
            let command_id = new_command_id("worker-upgrade")?;
            let protocol = lease.connection_mut().protocol_version();
            let operational = lease.connection_mut().operational();
            let preparation_failed = operational.harness_preparation_failed();
            let legacy_goal = (21..mj_core::relay::RELAY_IDLE_GOAL_INTENT_PROTOCOL)
                .contains(&protocol)
                && operational.facts().goal_active;
            if !preparation_failed && protocol >= 21 && !legacy_goal {
                if !lease
                    .connection_mut()
                    .reserve_idle(command_id.clone())
                    .await?
                {
                    lease.release();
                    return Ok(None);
                }
            } else {
                // Old ReserveIdle predicates refuse a saved goal even when no
                // work is running. Bootstrap with observed idle and a dispatch
                // barrier, not an atomic idle reservation. Failed preparation
                // also uses this path. Recheck work and the cut before swapping.
                lease
                    .connection_mut()
                    .submit(
                        command_id.clone(),
                        RelayCommand::BeginCheckpoint {
                            reason: Some("idle worker replacement".into()),
                        },
                    )
                    .await?;
            }
            loop {
                let mut snapshot = lease.connection_mut().sync().await?;
                let ready = checkpoint_barrier_is_ready(&snapshot, &command_id);
                let ready_cursor = if ready {
                    let cursor = snapshot
                        .operational
                        .checkpoint_ready
                        .clone()
                        .context("idle worker barrier lacks its ready cursor")?;
                    if let Err(error) =
                        validate_checkpoint_barrier_snapshot(&snapshot, &command_id, &cursor)
                    {
                        if error.is::<CheckpointDeferred>() {
                            return Ok(None);
                        }
                        return Err(error);
                    }
                    Some(cursor)
                } else {
                    None
                };
                snapshot.operational.checkpoint_barrier = None;
                if !snapshot.operational.safe_to_replace(harness) {
                    // Dropping the connection releases the barrier even if its
                    // acknowledgement was lost. No worker is stopped.
                    return Ok(None);
                }
                if let Some(ready_cursor) = ready_cursor {
                    // An accepted BeginCheckpoint has the same identity as
                    // ReserveIdle's barrier. Confirm it without the old goal
                    // predicate, while still checking private reviewer work.
                    // The pending/active barrier prevents new reviewer admission.
                    if legacy_goal
                        && !preparation_failed
                        && !lease
                            .connection_mut()
                            .reserve_idle(command_id.clone())
                            .await?
                    {
                        // Disconnect so the worker cancels the accepted barrier.
                        return Ok(None);
                    }
                    return Ok(Some(Self {
                        lease,
                        command_id,
                        harness,
                        ready_cursor,
                        _worker_owner: worker_owner,
                    }));
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("idle worker reservation timed out; worker was left running")?
    }

    pub(in crate::controller) fn protocol_version(&mut self) -> u32 {
        self.lease.connection_mut().protocol_version()
    }

    pub(in crate::controller) async fn verify_for_upgrade(&mut self) -> Result<bool> {
        let mut snapshot = self.lease.connection_mut().sync().await?;
        if !checkpoint_barrier_is_ready(&snapshot, &self.command_id)
            || snapshot.operational.checkpoint_ready.as_ref() != Some(&self.ready_cursor)
        {
            return Ok(false);
        }
        if let Err(error) =
            validate_checkpoint_barrier_snapshot(&snapshot, &self.command_id, &self.ready_cursor)
        {
            if error.is::<CheckpointDeferred>() {
                return Ok(false);
            }
            return Err(error);
        }
        snapshot.operational.checkpoint_barrier = None;
        Ok(snapshot.operational.safe_to_replace(self.harness))
    }

    pub(in crate::controller) fn finish_replacement(mut self, connection: StandaloneSession) {
        self.lease.replace_connection(connection);
        self.lease.release();
    }

    pub async fn acquire(handle: &ManagedSessionHandle, harness: HarnessKind) -> Result<Self> {
        let worker_owner = crate::worker_lifecycle::WorkerPermit::acquire(
            handle.session_id(),
            "workspace write",
            &ProcessExecutor,
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut lease = handle.lease_connection().await?;
            let snapshot = lease.connection_mut().sync().await?;
            ensure!(
                snapshot.operational.safe_to_replace(harness),
                "session must be live and idle with no queued or background work"
            );
            let command_id = new_command_id("workspace-write")?;
            lease
                .connection_mut()
                .submit(
                    command_id.clone(),
                    RelayCommand::BeginCheckpoint {
                        reason: Some("API workspace file write".into()),
                    },
                )
                .await?;
            loop {
                let snapshot = lease.connection_mut().sync().await?;
                if checkpoint_barrier_is_ready(&snapshot, &command_id) {
                    let ready_cursor = snapshot
                        .operational
                        .checkpoint_ready
                        .clone()
                        .context("file write barrier lacks its ready cursor")?;
                    let mut operation = Self {
                        lease,
                        command_id,
                        harness,
                        ready_cursor,
                        _worker_owner: worker_owner,
                    };
                    operation.verify().await?;
                    return Ok(operation);
                }
                // The bare flag misses a turn or a tool the projection has
                // not caught up with. The lease already holds the barrier, so
                // ask whether anything *else* is running.
                ensure!(
                    !snapshot.operational.has_work_in_flight(),
                    "session started work before the file barrier was ready"
                );
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("session did not become available for a file write within 30 seconds")?
    }

    pub async fn verify(&mut self) -> Result<()> {
        let mut snapshot = self.lease.connection_mut().sync().await?;
        validate_checkpoint_barrier_snapshot(&snapshot, &self.command_id, &self.ready_cursor)?;
        snapshot.operational.checkpoint_barrier = None;
        // Commands may queue behind this barrier, but cannot begin until it
        // releases. Their arrival does not invalidate an in-progress write.
        snapshot.operational.queued_prompts.clear();
        ensure!(
            snapshot.operational.safe_to_replace(self.harness),
            "session is no longer idle for the file write"
        );
        Ok(())
    }

    pub async fn release(mut self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(30), async {
            self.lease
                .connection_mut()
                .submit(
                    new_command_id("workspace-release")?,
                    RelayCommand::ReleaseCheckpoint {
                        barrier_command_id: self.command_id.clone(),
                    },
                )
                .await?;
            loop {
                let snapshot = self.lease.connection_mut().sync().await?;
                if snapshot.operational.checkpoint_barrier.as_deref() != Some(&self.command_id) {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("release file write barrier timed out")??;
        self.lease.release();
        Ok(())
    }
}

pub(super) fn checkpoint_barrier_is_ready(
    snapshot: &ManagedSessionSnapshot,
    command_id: &str,
) -> bool {
    snapshot.operational.checkpoint_barrier.as_deref() == Some(command_id)
        && snapshot.operational.checkpoint_ready.is_some()
}

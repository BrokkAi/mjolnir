use super::*;

/// An idle workspace operation holds the managed connection and a worker
/// barrier. Dropping this value disconnects and cancels the barrier; releasing
/// it resumes dispatch without claiming that an archive covers the journal.
pub struct IdleWorkspaceLease {
    pub(super) lease: ManagedSessionLease,
    pub(super) command_id: String,
    pub(super) harness: HarnessKind,
}

impl IdleWorkspaceLease {
    pub async fn acquire(handle: &ManagedSessionHandle, harness: HarnessKind) -> Result<Self> {
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
                    let mut operation = Self {
                        lease,
                        command_id,
                        harness,
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
        ensure!(
            checkpoint_barrier_is_ready(&snapshot, &self.command_id),
            "file write lost its workspace barrier"
        );
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

use super::*;

impl Controller {
    pub(in crate::controller) async fn checkpoint_session_latched(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: Option<&SessionManagerControl>,
        exclusivity: LatchExclusivity,
        export_policy: CheckpointExportPolicy,
    ) -> Result<LatchedCheckpoint> {
        self.checkpoint_session_latched_with_recovery_stage(
            session_id,
            executor,
            manager,
            exclusivity,
            export_policy,
            exclusivity == LatchExclusivity::HoldThroughClose,
        )
        .await
    }

    pub(super) async fn checkpoint_session_latched_with_recovery_stage(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        manager: Option<&SessionManagerControl>,
        exclusivity: LatchExclusivity,
        export_policy: CheckpointExportPolicy,
        recovery_copy: bool,
    ) -> Result<LatchedCheckpoint> {
        if let Some(operation) = crate::database::load_move_operation(session_id)?
            && operation.queue_admission_started
            && !operation.queue_admission_finished
        {
            // Advancing the recovery floor can prune terminal command IDs.
            // Keep them until a retained Move queue has been fully admitted.
            bail!(
                "move queue admission is incomplete; retry Move before checkpointing this destination"
            );
        }
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        session.validate_configuration(&self.config)?;
        let layout = self.session_export_layout(session_id, executor)?;
        let backend = layout.backend.clone();
        let profile = self
            .config
            .profiles
            .get(&session.last_profile)
            .context("session profile is missing")?;
        let reconnect = targets::reconnect_plan(&backend, session_id)?
            .commands
            .into_iter()
            .next()
            .context("reconnect plan is empty")?;
        let worker_root = targets::worker_root(&backend, session_id)?;
        let harness_home = target_profile_home(&backend, session_id, profile);
        let SessionExportLayout {
            workspace_root,
            primary_repository,
            repositories,
            ..
        } = layout;
        let target_path = |path: &str| match &backend {
            targets::TargetLocator::AwsEc2 { .. } | targets::TargetLocator::SshBare { .. }
                if !path.starts_with('/') =>
            {
                PathBuf::from(format!("~/{path}"))
            }
            _ => PathBuf::from(path),
        };
        // Packing can outlive the relay capture barrier. Another export must
        // never replace the archive whose digest this operation transfers.
        let operation_id = new_command_id("checkpoint")?;
        let remote_archive = format!("{worker_root}/{operation_id}.hel.zip");
        let remote_stage = format!("{worker_root}/{operation_id}-stage");
        let checkpointed_at = now();
        let target_manifest = TargetManifest {
            template_id: session.target_template_id.clone(),
            target_kind: backend.kind_name().into(),
            details: Default::default(),
        };
        let bundle_manifest = BundleManifest {
            id: session.bundle_id.clone(),
            primary_repository,
        };
        let session_manifest = |native_session_id: &str| SessionManifest {
            id: session.id.clone(),
            title: session.title.clone(),
            harness_kind: session.harness_kind,
            profile_id: session.last_profile.clone(),
            native_session_id: native_session_id.to_owned(),
            created_at: session.created_at.clone(),
            checkpointed_at: checkpointed_at.clone(),
            hel_version: env!("CARGO_PKG_VERSION").into(),
            relay_version: env!("CARGO_PKG_VERSION").into(),
            adapter_version: "acp-v1".into(),
        };
        let releases_after_capture = exclusivity == LatchExclusivity::ReleaseAfterLatch;
        if releases_after_capture
            && let Some(native_session_id) = session.native_session_id.as_deref()
        {
            let prestage = CheckpointCaptureSpec {
                protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
                session: session_manifest(native_session_id),
                target: target_manifest.clone(),
                bundle: bundle_manifest.clone(),
                relay_root: target_path(&worker_root),
                harness_home: target_path(&harness_home),
                workspace_root: target_path(&workspace_root),
                repositories: repositories.clone(),
                allow_empty_native: false,
                stage_path: target_path(&remote_stage),
                refresh_existing: false,
            };
            let prestage_started = Instant::now();
            let prestaged = {
                let _recovery_copy = recovery_copy
                    .then(|| ProvisionStageGuard::new(executor, ProvisionStage::RecoveryCopy));
                run_checkpoint_staging_command(
                    executor,
                    &backend,
                    session_id,
                    &prestage,
                    capture_stdin_command,
                    "prestage target checkpoint",
                    None,
                )
            };
            match prestaged {
                Ok(output) => match serde_json::from_slice::<CapturedCheckpoint>(&output.stdout) {
                    Ok(captured) => tracing::info!(
                        session_id,
                        prestage_ms = prestage_started.elapsed().as_millis() as u64,
                        native_bytes = captured.native_bytes,
                        repository_bytes = captured.repository_bytes,
                        reused_native = captured.reused_native,
                        "checkpoint target state prestaged while ACP dispatch remained active"
                    ),
                    Err(error) => tracing::warn!(
                        session_id,
                        error = format!("{error:#}"),
                        "checkpoint prestage returned an invalid result; barrier capture will replace it"
                    ),
                },
                Err(error) => {
                    if executor.cancellation_requested() {
                        return Err(error.context("checkpoint prestage was cancelled"));
                    }
                    tracing::warn!(
                        session_id,
                        error = format!("{error:#}"),
                        "checkpoint prestage failed; barrier capture will collect a fresh generation"
                    );
                }
            }
        }
        let (mut relay, mut restarted_worker) = self
            .open_checkpoint_relay(
                session_id,
                executor,
                manager,
                InstalledWorkerRestart {
                    backend: &backend,
                    worker_root: &worker_root,
                    reconnect: &reconnect,
                    launch: None,
                    prepared: false,
                    messages: &RESTART_FOR_CHECKPOINT,
                },
                exclusivity == LatchExclusivity::HoldThroughClose
                    || session.harness_kind != HarnessKind::Kimi,
            )
            .await?;
        let (barrier, barrier_command_id) = loop {
            // Restored native identity is not current-process readiness.
            // Startup gets its own cancellable budget; its timeout must not
            // enter the wedged-checkpoint worker-restart path below.
            let checkpoint_only = relay
                .connection_mut()
                .sync()
                .await?
                .operational
                .checkpoint_only;
            if !checkpoint_only {
                wait_for_native_session_in_stage(
                    relay.connection_mut(),
                    executor,
                    targets::ProvisionStage::Starting,
                )
                .await?;
            }
            if exclusivity == LatchExclusivity::HoldThroughClose {
                let snapshot = relay.connection_mut().sync().await?;
                if !checkpoint_only && snapshot.operational.capacity_retry.is_some() {
                    // An explicit stop/move cancels recovery before sealing the
                    // checkpoint. Routine recovery copies preserve its deadline.
                    relay
                        .connection_mut()
                        .submit(
                            new_command_id("cancel-capacity-retry")?,
                            RelayCommand::CancelTurn,
                        )
                        .await?;
                }
            }
            if exclusivity == LatchExclusivity::ReleaseAfterLatch {
                let snapshot = relay.connection_mut().sync().await?;
                if let Some(wait) = snapshot
                    .operational
                    .routine_checkpoint_wait(session.harness_kind)
                {
                    // The same answer the recovery coordinator acted on, asked
                    // again because the session can start working after the
                    // observation. A routine checkpoint must not open a
                    // barrier just to abandon it, so this defers before
                    // BeginCheckpoint is submitted. Close deliberately does
                    // not use this path and may interrupt the work instead.
                    relay.release();
                    return Err(CheckpointDeferred::from(wait).into());
                }
            }
            let barrier_command_id = new_command_id("checkpoint")?;
            let timeout = if restarted_worker {
                CHECKPOINT_BARRIER_TIMEOUT_AFTER_RESTART
            } else {
                CHECKPOINT_BARRIER_TIMEOUT
            };
            let result = {
                let connection = relay.connection_mut();
                connection
                    .submit(
                        barrier_command_id.clone(),
                        RelayCommand::BeginCheckpoint {
                            reason: Some("controller archive checkpoint".into()),
                        },
                    )
                    .await?;
                wait_for_checkpoint_barrier(
                    connection,
                    session_id,
                    &barrier_command_id,
                    timeout,
                    BarrierBusyPolicy::of(exclusivity),
                    session.harness_kind,
                )
                .await
            };
            match result {
                Ok(barrier) => break (barrier, barrier_command_id),
                Err(error)
                    if !restarted_worker && checkpoint_barrier_needs_worker_restart(&error) =>
                {
                    if exclusivity == LatchExclusivity::ReleaseAfterLatch
                        && matches!(session.harness_kind, HarnessKind::Kimi | HarnessKind::Codex)
                    {
                        let safe_to_restart =
                            relay.connection_mut().sync().await.is_ok_and(|snapshot| {
                                snapshot.operational.safe_to_replace(session.harness_kind)
                            });
                        if !safe_to_restart {
                            return Err(error.context(CheckpointDeferred::background_work()));
                        }
                    }
                    tracing::warn!(
                        session_id,
                        "checkpoint requires a worker restart; restarting and retrying: {error:#}"
                    );
                    let restarted = self
                        .restart_worker_for_checkpoint(
                            session_id,
                            executor,
                            &backend,
                            &worker_root,
                            &reconnect,
                        )
                        .await;
                    let connection = match restarted {
                        Ok(connection) => connection,
                        Err(restart)
                            if restart_falls_back_to_checkpoint_only(exclusivity, &restart) =>
                        {
                            tracing::warn!(
                                session_id,
                                "the worker restarted for the checkpoint did not come back; checkpointing its durable state without starting the harness: {restart:#}"
                            );
                            executor.notify_notice(
                                "Saving the session without starting its harness, which did not come back",
                            );
                            self.restart_worker_for_checkpoint_only(
                                session_id,
                                executor,
                                &backend,
                                &worker_root,
                                &reconnect,
                            )
                            .await
                            .map_err(|fallback| {
                                fallback.context(format!(
                                    "the worker restart for the checkpoint failed first: {restart:#}"
                                ))
                            })?
                        }
                        Err(restart) => return Err(restart),
                    };
                    relay.replace_connection(connection);
                    restarted_worker = true;
                }
                Err(error) => return Err(error),
            }
        };
        let barrier_ready_at = Instant::now();
        // Project memory is checkpoint state, not relay connection state.
        // Reconcile it once while the checkpoint barrier keeps the harness
        // idle. Ordinary attach and polling deliberately never touch it.
        relay
            .connection_mut()
            .sync_project_memory()
            .await
            .context("synchronize project memory for checkpoint")?;
        let cursor = barrier
            .operational
            .checkpoint_ready
            .clone()
            .context("relay reported a checkpoint barrier without its ready cursor")?;
        // The live view is windowed. An archive must contain the complete
        // durable conversation at the verified checkpoint cut.
        let materialized = if barrier.window.omitted_items > 0 {
            let session_id = barrier.materialized.session_id.clone();
            tokio::task::spawn_blocking(move || {
                crate::database::load_materialized_session(&session_id)?
                    .context("checkpoint durable projection disappeared")
            })
            .await
            .context("checkpoint history load task failed")??
        } else {
            barrier.materialized
        };
        let expected_ordinal = materialized.applied_event_ordinal;
        let expected_digest = materialized.applied_event_digest.clone();
        ensure!(
            expected_ordinal == barrier.operational.latest_ordinal,
            "checkpoint projection frontier {expected_ordinal} does not match relay frontier {}",
            barrier.operational.latest_ordinal
        );
        ensure!(
            expected_digest == barrier.operational.latest_digest,
            "checkpoint projection digest does not match the relay frontier digest"
        );
        ensure_exact_checkpoint_cut(&cursor, expected_ordinal, &expected_digest)?;
        let canonical_session = canonical_session_from_materialized(&materialized)?;
        let native_session_id = barrier
            .operational
            .native_session_id
            .or_else(|| session.native_session_id.clone())
            .context("harness did not report its native session ID")?;

        // The latch holds: this projection sits exactly at the barrier's ready
        // cursor. Exporting and transferring the archive needs the barrier, not
        // the connection, so hand it back and let the dashboard keep syncing
        // and submitting while the slow phase runs.
        if exclusivity == LatchExclusivity::ReleaseAfterLatch {
            relay.end_latch();
        }

        // Reuse before exporting: verifying an installed archive costs far less
        // than exporting and transferring an identical one. A reused archive's
        // frontier trails the cursor its caller seals by the checkpoint's own
        // bookkeeping events, and only by those; resume rolls the controller's
        // projection back to the archived record.
        if export_policy == CheckpointExportPolicy::ReuseUnchangedArchive
            // Host worktree edits do not advance the relay frontier. Always
            // recapture before retiring one, including archives written by
            // older workers that only recorded its Git metadata.
            && session.managed_worktree.is_none()
            && let Some(artifact) = reusable_installed_checkpoint(
                session_id,
                session.checkpoint.as_ref(),
                &native_session_id,
                cursor.ordinal,
                &canonical_session,
            )
        {
            return Ok(LatchedCheckpoint {
                artifact,
                relay,
                barrier_command_id,
                cursor,
                completion: CheckpointCompletion::HeldBarrier,
            });
        }

        // Close must keep ACP dispatch frozen until it seals the relay, so only
        // an ordinary checkpoint may hand dispatch back at the end of its
        // export. `completion` also records whether an error path still has a
        // barrier to cancel.
        let mut completion = CheckpointCompletion::HeldBarrier;

        let exported: Result<CheckpointArtifact> = async {
            let spec = CheckpointExportSpec {
                protocol_version: CHECKPOINT_EXPORT_PROTOCOL_VERSION,
                session: session_manifest(&native_session_id),
                target: target_manifest,
                bundle: bundle_manifest,
                relay_root: target_path(&worker_root),
                harness_home: target_path(&harness_home),
                workspace_root: target_path(&workspace_root),
                repositories,
                canonical_session,
                output_path: target_path(&remote_archive),
            };
            // Only the single-shot export path measures itself here; the
            // capture/pack path already logs its own phases above.
            let mut export_ms: Option<u64> = None;
            let exported = if releases_after_capture {
                let capture_spec = CheckpointCaptureSpec {
                    protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
                    session: spec.session.clone(),
                    target: spec.target.clone(),
                    bundle: spec.bundle.clone(),
                    relay_root: spec.relay_root.clone(),
                    harness_home: spec.harness_home.clone(),
                    workspace_root: spec.workspace_root.clone(),
                    repositories: spec.repositories.clone(),
                    allow_empty_native: !current_native_session_received_prompt(
                        &spec.canonical_session,
                    ),
                    stage_path: target_path(&remote_stage),
                    refresh_existing: true,
                };
                let capture_started = Instant::now();
                let captured = {
                    let _recovery_copy = recovery_copy.then(|| {
                        ProvisionStageGuard::new(executor, ProvisionStage::RecoveryCopy)
                    });
                    run_checkpoint_staging_command(
                        executor,
                        &backend,
                        session_id,
                        &capture_spec,
                        capture_stdin_command,
                        "capture target checkpoint",
                        None,
                    )?
                };
                let captured: CapturedCheckpoint = serde_json::from_slice(&captured.stdout)
                    .context("decode captured checkpoint result")?;
                tracing::info!(
                    session_id,
                    capture_ms = capture_started.elapsed().as_millis() as u64,
                    barrier_held_ms = barrier_ready_at.elapsed().as_millis() as u64,
                    native_bytes = captured.native_bytes,
                    repository_bytes = captured.repository_bytes,
                    reused_native = captured.reused_native,
                    "checkpoint target state captured; releasing ACP dispatch"
                );
                completion = release_checkpoint_after_capture(
                    &mut relay,
                    session_id,
                    &barrier_command_id,
                    &cursor,
                    session.harness_kind,
                )
                .await?;
                let pack_spec = CheckpointPackSpec {
                    protocol_version: CHECKPOINT_STAGING_PROTOCOL_VERSION,
                    relay_root: spec.relay_root.clone(),
                    stage_path: target_path(&remote_stage),
                    canonical_session: spec.canonical_session.clone(),
                    output_path: spec.output_path.clone(),
                };
                let pack_started = Instant::now();
                let output = {
                    let _recovery_copy = recovery_copy.then(|| {
                        ProvisionStageGuard::new(executor, ProvisionStage::RecoveryCopy)
                    });
                    run_checkpoint_staging_command(
                        executor,
                        &backend,
                        session_id,
                        &pack_spec,
                        pack_stdin_command,
                        "pack target checkpoint",
                        None,
                    )?
                };
                tracing::info!(
                    session_id,
                    pack_ms = pack_started.elapsed().as_millis() as u64,
                    "checkpoint archive packaged after ACP dispatch resumed"
                );
                output
            } else {
                let export_started = Instant::now();
                let output = {
                    let _recovery_copy = recovery_copy.then(|| {
                        ProvisionStageGuard::new(executor, ProvisionStage::RecoveryCopy)
                    });
                    run_checkpoint_staging_command(
                        executor,
                        &backend,
                        session_id,
                        &spec,
                        export_stdin_command,
                        "export target checkpoint",
                        None,
                    )?
                };
                export_ms = Some(export_started.elapsed().as_millis() as u64);
                output
            };
            let target_checkpoint: mj_checkpoint::checkpoint::TargetCheckpoint =
                serde_json::from_slice(&exported.stdout)
                    .context("decode target checkpoint result")?;
            if let Some(export_ms) = export_ms {
                // A worker that predates the timings field reports nothing, so
                // the phase numbers read as zero; `timings_reported` says which.
                let timings = target_checkpoint.timings.unwrap_or_default();
                tracing::info!(
                    session_id,
                    export_ms,
                    timings_reported = target_checkpoint.timings.is_some(),
                    native_ms = timings.native_ms,
                    repositories_ms = timings.repositories_ms,
                    archive_ms = timings.archive_ms,
                    worker_total_ms = timings.total_ms,
                    "checkpoint archive exported on the target"
                );
            }
            if target_checkpoint.event_frontier != expected_ordinal {
                bail!(
                    "target checkpoint event frontier changed: expected {expected_ordinal}, found {}",
                    target_checkpoint.event_frontier
                );
            }
            if target_checkpoint.event_frontier_digest != expected_digest {
                bail!("target checkpoint event frontier digest changed");
            }

            // Checkpoint archives are immutable once controller metadata points
            // at them. A repeated checkpoint may have the same event frontier,
            // so a frontier-only name could overwrite the last known-good
            // archive before the metadata swap commits.
            let archive_id = new_command_id("archive")?;
            let destination = sessions_dir().join(format!(
                "{session_id}-{}-{archive_id}.hel.zip",
                target_checkpoint.event_frontier
            ));
            let transfer = CheckpointTransfer {
                locator: &backend,
                session_id,
                operation_id: &operation_id,
                remote_archive: &remote_archive,
                destination: &destination,
                expected_sha256: &target_checkpoint.sha256,
                expected_event_frontier: target_checkpoint.event_frontier,
                expected_event_frontier_digest: &target_checkpoint.event_frontier_digest,
            };
            let metadata = {
                let _verifying = ProvisionStageGuard::new(executor, ProvisionStage::Verifying);
                let transfer_started = Instant::now();
                let verified = transfer.execute(executor)?;
                tracing::info!(
                    session_id,
                    transfer_and_checksum_ms = transfer_started.elapsed().as_millis() as u64,
                    "checkpoint archive transferred and checksum-verified"
                );
                let installed_archive = verified.archive_path().to_path_buf();
                let validate_transferred = || -> Result<()> {
                    ensure!(
                        verified.sha256() == target_checkpoint.sha256,
                        "target and controller checkpoint checksums differ"
                    );
                    ensure!(
                        verified.event_frontier_digest() == expected_digest,
                        "verified checkpoint event frontier digest changed"
                    );
                    Ok(())
                };
                if let Err(error) = validate_transferred() {
                    return Err(remove_uninstalled_checkpoint(&installed_archive, error));
                }
                // A checkpoint that still holds its barrier proves workspace
                // consistency here instead. One that already released proved it
                // before releasing; the sha256 chain covers the transfer itself.
                if completion == CheckpointCompletion::HeldBarrier {
                    let revalidated = relay.sync_snapshot().await.and_then(|snapshot| {
                        if releases_after_capture {
                            validate_automatic_checkpoint_barrier_snapshot(
                                &snapshot,
                                &barrier_command_id,
                                &cursor,
                                session.harness_kind,
                            )
                        } else {
                            validate_checkpoint_barrier_snapshot(
                                &snapshot,
                                &barrier_command_id,
                                &cursor,
                            )
                        }
                    });
                    if let Err(error) = revalidated {
                        return Err(remove_uninstalled_checkpoint(
                            &installed_archive,
                            error.context(
                                "checkpoint barrier changed while transferring its archive",
                            ),
                        ));
                    }
                }
                if let Err(error) = transfer
                    .cleanup_plan(&verified)
                    .and_then(|plan| plan.execute(executor).map(|_| ()))
                {
                    return Err(remove_uninstalled_checkpoint(
                        &installed_archive,
                        error.context("clean target checkpoint staging"),
                    ));
                }
                CheckpointMetadata {
                    archive_path: verified.archive_path().to_path_buf(),
                    sha256: verified.sha256().to_string(),
                    created_at: checkpointed_at.clone(),
                    event_frontier: verified.event_frontier(),
                }
            };
            Ok(CheckpointArtifact {
                metadata,
                native_session_id,
                event_frontier_digest: expected_digest,
            })
        }
        .await;

        let artifact = match exported {
            Ok(artifact) => artifact,
            Err(error) => {
                // The barrier freezes ACP dispatch until it ends. Nothing will
                // complete it now, and the connection that opened it is back
                // with the session actor, so cancel it instead of leaving the
                // harness frozen until that connection happens to drop. A
                // barrier released after the export is already gone.
                if completion == CheckpointCompletion::HeldBarrier
                    && let Err(cancel_error) = relay.cancel_abandoned_barrier().await
                {
                    tracing::warn!(
                        session_id,
                        "failed checkpoint could not cancel its relay barrier: {cancel_error:#}"
                    );
                }
                return Err(error);
            }
        };
        Ok(LatchedCheckpoint {
            artifact,
            relay,
            barrier_command_id,
            cursor,
            completion,
        })
    }
}

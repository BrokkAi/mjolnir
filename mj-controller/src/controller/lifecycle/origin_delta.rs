use super::*;
use crate::controller::worktree::ExecutorGit;
use mj_core::worker_launch::{WorkerLaunchConfig, WorkerRunMode};
use mj_core::worker_suspend::{self as wire, SuspendReceipt, SuspendRequest};
use std::path::Path;

const MISSING_WIRE_EXIT_STATUS: i32 = 44;

pub(super) struct OriginDeltaSuspend {
    session_id: String,
    backend: targets::TargetLocator,
    root: String,
    checkout: PathBuf,
    child_sessions: Vec<String>,
    operation: String,
    request_recorded: bool,
    accepted: bool,
}

fn read_wire_bytes(
    backend: &targets::TargetLocator,
    session_id: &str,
    root: &str,
    name: &str,
    executor: &(impl CommandExecutor + Sync),
) -> Result<Option<Vec<u8>>> {
    if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
        let path = Path::new(root).join(name);
        return match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
        };
    }
    let path = format!("{root}/{name}");
    let script = format!(
        "path={}; if [ -e \"$path\" ]; then exec cat -- \"$path\"; fi; exit {MISSING_WIRE_EXIT_STATUS}",
        targets::posix_quote(&path),
    );
    let command = targets::command_on_locator(
        targets::SshTrafficClass::Control,
        backend,
        session_id,
        vec!["sh".into(), "-c".into(), script],
        format!("read suspend record {name}"),
    )?;
    let output = mj_core::runtime::off_async_worker(|| executor.execute(&command))?;
    if output.status == 0 {
        Ok(Some(output.stdout))
    } else if output.status == MISSING_WIRE_EXIT_STATUS {
        Ok(None)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "read suspend record {name} failed (status {}): {}",
            output.status,
            stderr.trim()
        );
    }
}

fn read_wire<T: serde::de::DeserializeOwned>(
    backend: &targets::TargetLocator,
    session_id: &str,
    root: &str,
    name: &str,
    executor: &(impl CommandExecutor + Sync),
) -> Result<Option<T>> {
    let Some(bytes) = read_wire_bytes(backend, session_id, root, name, executor)? else {
        return Ok(None);
    };
    Ok(Some(
        serde_json::from_slice(&bytes).with_context(|| format!("decode {root}/{name}"))?,
    ))
}

fn write_wire<T: serde::Serialize>(
    backend: &targets::TargetLocator,
    session_id: &str,
    root: &str,
    name: &str,
    value: &T,
    executor: &(impl CommandExecutor + Sync),
) -> Result<()> {
    if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
        return wire::write(Path::new(root), name, value);
    }
    let path = format!("{root}/{name}");
    let tmp = format!("{path}.tmp.$$");
    let script = format!(
        "path={}; tmp={}; mkdir -p -- \"$(dirname -- \"$path\")\" && cat > \"$tmp\" && mv -f -- \"$tmp\" \"$path\"",
        targets::posix_quote(&path),
        targets::posix_quote(&tmp),
    );
    let command = targets::command_on_locator(
        targets::SshTrafficClass::Control,
        backend,
        session_id,
        vec!["sh".into(), "-c".into(), script],
        format!("write suspend record {name}"),
    )?
    .with_sensitive_stdin(serde_json::to_vec(value)?);
    mj_core::runtime::off_async_worker(|| super::super::execute_checked(executor, command))?;
    Ok(())
}

fn remove_wire_files(
    backend: &targets::TargetLocator,
    session_id: &str,
    root: &str,
    names: &[&str],
    executor: &(impl CommandExecutor + Sync),
) -> Result<()> {
    if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
        for name in names {
            match std::fs::remove_file(Path::new(root).join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("remove suspend record {name}"));
                }
            }
        }
        return Ok(());
    }
    let mut args = vec!["rm".to_owned(), "-f".to_owned(), "--".to_owned()];
    for name in names {
        args.push(format!("{root}/{name}"));
    }
    let command = targets::command_on_locator(
        targets::SshTrafficClass::Control,
        backend,
        session_id,
        args,
        "clear worker suspend records",
    )?;
    mj_core::runtime::off_async_worker(|| super::super::execute_checked(executor, command))?;
    Ok(())
}

fn read_launch_config(
    backend: &targets::TargetLocator,
    session_id: &str,
    root: &str,
    executor: &(impl CommandExecutor + Sync),
) -> Result<WorkerLaunchConfig> {
    if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
        return WorkerLaunchConfig::read(&Path::new(root).join("launch.json"));
    }
    read_wire::<WorkerLaunchConfig>(backend, session_id, root, "launch.json", executor)?
        .with_context(|| format!("read {root}/launch.json: file not found"))
}

fn worker_identity_is_alive(
    backend: &targets::TargetLocator,
    session_id: &str,
    root: &str,
    worker: wire::WorkerIdentity,
    executor: &(impl CommandExecutor + Sync),
    allow_unsupported: bool,
) -> Result<bool> {
    if matches!(backend, targets::TargetLocator::LocalBare { .. }) {
        return mj_core::runtime::off_async_worker(|| worker.is_alive());
    }
    let command = targets::command_on_locator(
        targets::SshTrafficClass::Control,
        backend,
        session_id,
        vec![
            format!("{root}/hel"),
            "worker".into(),
            "identity-alive".into(),
            "--pid".into(),
            worker.pid.to_string(),
            "--start".into(),
            worker.start.to_string(),
        ],
        "check suspend worker liveness",
    )?;
    let output = mj_core::runtime::off_async_worker(|| executor.execute(&command))?;
    if output.status != 0 {
        if allow_unsupported {
            return Ok(false);
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "check suspend worker liveness failed (status {}): {}",
            output.status,
            stderr.trim()
        );
    }
    match output.stdout.as_slice() {
        b"alive\n" => Ok(true),
        b"dead\n" => Ok(false),
        other => anyhow::bail!(
            "unexpected worker identity-alive output: {:?}",
            String::from_utf8_lossy(other)
        ),
    }
}

fn checkout_has_origin_remote(
    git: &impl mj_checkpoint::archive::GitCommandRunner,
    repository: &Path,
) -> Result<bool> {
    let origin = git.run(
        repository,
        &mj_checkpoint::archive::GitCommand {
            arguments: ["config", "--get", "remote.origin.url"]
                .map(Into::into)
                .into(),
            ..Default::default()
        },
    )?;
    Ok(origin.status == 0 && !origin.stdout.trim_ascii().is_empty())
}

pub(in crate::controller) fn managed_origin_override(
    worktree: &mj_core::state::ManagedWorktree,
) -> Option<String> {
    if !matches!(
        worktree.target,
        mj_core::state::ManagedWorktreeTarget::Local
    ) {
        return None;
    }
    mj_core::runtime::off_async_worker(|| {
        use mj_checkpoint::archive::GitCommandRunner;
        let origin = mj_checkpoint::archive::SystemGit
            .run(
                &worktree.worktree_root,
                &mj_checkpoint::archive::GitCommand {
                    arguments: ["config", "--get", "remote.origin.url"]
                        .map(Into::into)
                        .into(),
                    ..Default::default()
                },
            )
            .ok()?;
        if origin.status == 0 && !origin.stdout.trim_ascii().is_empty() {
            return None;
        }
        if !worktree.source_repository.exists() {
            return None;
        }
        let git_dir = mj_checkpoint::archive::SystemGit
            .run(
                &worktree.source_repository,
                &mj_checkpoint::archive::GitCommand {
                    arguments: ["rev-parse", "--git-dir"].map(Into::into).into(),
                    ..Default::default()
                },
            )
            .ok()?;
        if git_dir.status == 0 {
            Some(worktree.source_repository.to_string_lossy().into_owned())
        } else {
            None
        }
    })
}

const ALL_SUSPEND_WIRE_FILES: &[&str] = &[
    wire::ACCEPTED,
    wire::REQUEST,
    wire::RECEIPT,
    wire::PROGRESS,
    wire::RESTART,
];

/// Clear any OriginDelta suspend request, receipt, and progress wire files left
/// in `backend`'s worker root for `session_id` so a subsequent Resume and later
/// Suspend cannot mistake an abandoned Move's records for a fresh quiescence.
pub(in crate::controller) fn clear_suspend_records_at_target(
    backend: &targets::TargetLocator,
    session_id: &str,
    executor: &(impl CommandExecutor + Sync),
) -> Result<()> {
    let root = targets::worker_root(backend, session_id)?;
    remove_wire_files(backend, session_id, &root, ALL_SUSPEND_WIRE_FILES, executor)
}

impl OriginDeltaSuspend {
    /// Whether this suspend captures OriginDelta. An accepted suspend keeps
    /// the capture it started with, even after its worker has exited. A new
    /// one applies to any managed checkout (local bare, SSH bare, or container
    /// workspace) with an `origin` remote or local source repository to delta
    /// against; if the running worker predates `suspend-worker.json`, suspend
    /// upgrades it in place instead of falling back to version-2 capture.
    pub(super) fn applies(
        controller: &Controller,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<bool> {
        let (backend, root) = controller.worker_placement(session_id)?;
        if read_wire::<String>(&backend, session_id, &root, wire::ACCEPTED, executor)?.is_some() {
            return Ok(true);
        }
        let checkout = controller.state.checkout(session_id)?;
        let git = ExecutorGit::for_session(executor, &backend, session_id);
        match checkout {
            mj_core::state::Checkout::ManagedWorktree { worktree, .. } => {
                if !worktree.uses_origin_delta_suspend() {
                    return Ok(false);
                }
                let has_origin = mj_core::runtime::off_async_worker(|| {
                    checkout_has_origin_remote(&git, &worktree.worktree_root)
                })?;
                if has_origin {
                    return Ok(true);
                }
                Ok(managed_origin_override(worktree).is_some())
            }
            mj_core::state::Checkout::ManagedWorkspace => {
                let session = controller
                    .state
                    .sessions
                    .get(session_id)
                    .with_context(|| format!("unknown session {session_id}"))?;
                let Some(bundle) = session.project_bundle(&controller.config) else {
                    return Ok(false);
                };
                if bundle.repositories.is_empty() {
                    return Ok(false);
                }
                let workspace_root = PathBuf::from(crate::controller::network_git::workspace_root(
                    &backend,
                    session.container_workspace.as_deref(),
                ));
                mj_core::runtime::off_async_worker(|| {
                    for repository in &bundle.repositories {
                        let repo_path = workspace_root.join(&repository.destination);
                        if !checkout_has_origin_remote(&git, &repo_path)? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                })
            }
            mj_core::state::Checkout::Attached { .. }
            | mj_core::state::Checkout::Borrowed { .. } => Ok(false),
        }
    }

    /// Whether the session's live worker advertises origin-delta quiescence
    /// capability (`suspend-worker.json` with a live PID in the target
    /// namespace).
    pub(super) fn worker_capable(&self, executor: &(impl CommandExecutor + Sync)) -> Result<bool> {
        match read_wire::<wire::WorkerIdentity>(
            &self.backend,
            &self.session_id,
            &self.root,
            wire::CAPABILITY,
            executor,
        )? {
            Some(worker) => worker_identity_is_alive(
                &self.backend,
                &self.session_id,
                &self.root,
                worker,
                executor,
                true,
            ),
            None => Ok(false),
        }
    }

    pub(super) fn load(
        controller: &Controller,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<Self> {
        let session = controller
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let checkout = controller.state.checkout(session_id)?;
        let (backend, root) = controller.worker_placement(session_id)?;
        let checkout_path = match checkout {
            mj_core::state::Checkout::ManagedWorktree { worktree, .. } => {
                worktree.worktree_root.clone()
            }
            mj_core::state::Checkout::ManagedWorkspace => {
                PathBuf::from(crate::controller::network_git::workspace_root(
                    &backend,
                    session.container_workspace.as_deref(),
                ))
            }
            mj_core::state::Checkout::Attached { path } => path.to_path_buf(),
            _ => bail!("managed checkout is missing"),
        };
        let accepted = read_wire::<String>(&backend, session_id, &root, wire::ACCEPTED, executor)?;
        let request =
            read_wire::<SuspendRequest>(&backend, session_id, &root, wire::REQUEST, executor)?
                .filter(|request| accepted.as_ref() == Some(&request.operation));
        let request_recorded = request.is_some();
        let child_sessions = if let Some(request) = request {
            ensure!(
                request.checkout == checkout_path,
                "accepted suspend request belongs to a different checkout"
            );
            // A stopped borrower can still have detached writers. Keep the
            // scope of the original stop after its worker has exited.
            request.child_sessions
        } else {
            crate::database::environment_quiescence_session_ids(session_id)?
        };
        Ok(Self {
            session_id: session_id.to_owned(),
            backend,
            root,
            checkout: checkout_path,
            child_sessions,
            request_recorded,
            accepted: accepted.is_some(),
            operation: match accepted {
                Some(operation) => operation,
                None => new_command_id("origin-delta-suspend")?,
            },
        })
    }

    pub(super) fn accepted(&self) -> bool {
        self.accepted
    }

    pub(super) fn already_quiescent(&self) -> bool {
        self.operation.starts_with("parked-origin-delta-suspend-")
    }

    pub(super) fn accept(
        &mut self,
        executor: &(impl CommandExecutor + Sync),
        parked: bool,
    ) -> Result<()> {
        self.operation = new_command_id(if parked {
            "parked-origin-delta-suspend"
        } else {
            "origin-delta-suspend"
        })?;
        write_wire(
            &self.backend,
            &self.session_id,
            &self.root,
            wire::ACCEPTED,
            &self.operation,
            executor,
        )?;
        self.request_recorded = false;
        self.accepted = true;
        Ok(())
    }

    pub(super) fn request_recorded(&self) -> bool {
        self.request_recorded
    }

    pub(super) fn capture_scope(&self) -> wire::SuspendCapture {
        wire::SuspendCapture {
            operation: self.operation.clone(),
            checkout: self.checkout.clone(),
            child_sessions: self.child_sessions.clone(),
        }
    }

    pub(super) fn restart_reason(
        &self,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<Option<String>> {
        Ok(read_wire::<SuspendReceipt>(
            &self.backend,
            &self.session_id,
            &self.root,
            wire::RESTART,
            executor,
        )?
        .filter(|receipt| receipt.operation == self.operation)
        .and_then(|receipt| receipt.error))
    }

    pub(super) fn record_restart(
        &self,
        error: &anyhow::Error,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        write_wire(
            &self.backend,
            &self.session_id,
            &self.root,
            wire::RESTART,
            &SuspendReceipt {
                operation: self.operation.clone(),
                error: Some(format!("{error:#}")),
            },
            executor,
        )
    }

    pub(super) fn clear_after_restart(
        &self,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        remove_wire_files(
            &self.backend,
            &self.session_id,
            &self.root,
            ALL_SUSPEND_WIRE_FILES,
            executor,
        )
    }

    pub(super) fn receipt(
        &self,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<Option<SuspendReceipt>> {
        Ok(read_wire::<SuspendReceipt>(
            &self.backend,
            &self.session_id,
            &self.root,
            wire::RECEIPT,
            executor,
        )?
        .filter(|receipt| receipt.operation == self.operation))
    }

    pub(super) fn request_stop(&mut self, executor: &(impl CommandExecutor + Sync)) -> Result<()> {
        if self.receipt(executor)?.is_some() {
            return Ok(());
        }
        let mut args = vec![
            format!("{}/hel", self.root),
            "worker".to_owned(),
            "quiesce".to_owned(),
            "--root".to_owned(),
            self.root.clone(),
            "--operation".to_owned(),
            self.operation.clone(),
            "--checkout".to_owned(),
            self.checkout.to_string_lossy().into_owned(),
        ];
        for id in &self.child_sessions {
            args.extend(["--child-session".to_owned(), id.clone()]);
        }
        let command = targets::command_on_locator(
            targets::SshTrafficClass::Control,
            &self.backend,
            &self.session_id,
            args,
            "request managed checkout process quiescence",
        )?;
        if let Err(error) =
            mj_core::runtime::off_async_worker(|| super::super::execute_checked(executor, command))
        {
            // Delivery can fail after the worker's durable request is written.
            match read_wire::<SuspendRequest>(
                &self.backend,
                &self.session_id,
                &self.root,
                wire::REQUEST,
                executor,
            ) {
                Ok(Some(request)) if request.operation == self.operation => {}
                Ok(_) => return Err(error),
                Err(readback) => {
                    return Err(error
                        .context(format!(
                            "could not confirm delivery of the stop request: {readback:#}"
                        ))
                        .context(LifecycleFailureReason::SuspendPending));
                }
            }
        }
        self.request_recorded = true;
        Ok(())
    }

    pub(super) async fn wait_for_stop(
        &self,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        self.wait_for_stop_with_uncancelled(executor, &ProcessExecutor)
            .await
            .context(LifecycleFailureReason::SuspendPending)
    }

    pub(super) async fn wait_for_stop_with_uncancelled(
        &self,
        executor: &(impl CommandExecutor + Sync),
        uncancelled: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        executor.begin_resumable_work()?;
        let outcome = async {
            let initial_cancelled =
                executor.cancellation_requested() && !executor.shutdown_interrupted();
            let worker = if initial_cancelled {
                read_wire::<SuspendRequest>(
                    &self.backend,
                    &self.session_id,
                    &self.root,
                    wire::REQUEST,
                    uncancelled,
                )?
            } else {
                read_wire::<SuspendRequest>(
                    &self.backend,
                    &self.session_id,
                    &self.root,
                    wire::REQUEST,
                    executor,
                )?
            }
            .context("worker did not record the suspend request")?
            .worker;
            let mut cancellation_recorded = if initial_cancelled {
                self.restart_reason(uncancelled)?.is_some()
            } else {
                self.restart_reason(executor)?.is_some()
            };
            let mut displayed_notice = String::new();
            loop {
                ensure!(
                    !executor.shutdown_interrupted(),
                    "suspend process wait was handed off"
                );
                if executor.cancellation_requested() && !cancellation_recorded {
                    self.record_restart(&anyhow::anyhow!("suspend was cancelled"), uncancelled)?;
                    cancellation_recorded = true;
                }
                let use_uncancelled = cancellation_recorded || executor.cancellation_requested();
                // A receipt can precede the worker's last cleanup tasks.
                let poll_executor_alive = if use_uncancelled {
                    worker_identity_is_alive(
                        &self.backend,
                        &self.session_id,
                        &self.root,
                        worker,
                        uncancelled,
                        false,
                    )?
                } else {
                    worker_identity_is_alive(
                        &self.backend,
                        &self.session_id,
                        &self.root,
                        worker,
                        executor,
                        false,
                    )?
                };
                if !poll_executor_alive {
                    return Ok(());
                }
                let mut notice = if cancellation_recorded {
                    "Restarting after the previous agent exits".to_owned()
                } else {
                    "Waiting for the session's processes to stop".to_owned()
                };
                let progress = if use_uncancelled {
                    read_wire::<wire::SuspendProgress>(
                        &self.backend,
                        &self.session_id,
                        &self.root,
                        wire::PROGRESS,
                        uncancelled,
                    )?
                } else {
                    read_wire::<wire::SuspendProgress>(
                        &self.backend,
                        &self.session_id,
                        &self.root,
                        wire::PROGRESS,
                        executor,
                    )?
                };
                if let Some(progress) = progress
                    && progress.operation == self.operation
                    && !progress.remaining_pids.is_empty()
                {
                    let pids = progress
                        .remaining_pids
                        .iter()
                        .map(u32::to_string)
                        .collect::<Vec<_>>();
                    notice.push_str(&format!(" (PIDs {})", pids.join(", ")));
                }
                if displayed_notice != notice {
                    executor.notify_notice(&notice);
                    displayed_notice = notice;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        .await;
        executor.end_resumable_work("record suspend quiescence outcome")?;
        outcome
    }

    pub(super) fn live_worker_mode(
        &self,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<Option<WorkerRunMode>> {
        let existing = mj_core::runtime::off_async_worker(|| {
            super::super::worker_binary::probe_worker_liveness(
                executor,
                &self.backend,
                &self.session_id,
                &self.root,
            )
        })?;
        ensure!(
            existing.status == 0,
            "cannot verify the retained worker's liveness: {}",
            String::from_utf8_lossy(&existing.stderr).trim()
        );
        match existing.stdout.as_slice() {
            b"dead\n" => Ok(None),
            b"alive\n" | b"starting\n" => {
                read_launch_config(&self.backend, &self.session_id, &self.root, executor)
                    .map(|launch| Some(launch.run_mode))
            }
            _ => bail!("unexpected retained worker liveness response"),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn start_worker<'a>(
        &'a self,
        controller: &'a Controller,
        admission: &'a crate::recovery_gate::SessionAdmission,
        session_id: &'a str,
        executor: &'a (impl CommandExecutor + Sync),
        manager: Option<&'a SessionManagerControl>,
        run_mode: WorkerRunMode,
        reattach_only: bool,
    ) -> std::pin::Pin<
        Box<impl Future<Output = Result<super::super::checkpoint::ControllerRelayLease>> + 'a>,
    > {
        use super::super::checkpoint::adopt_restarted_checkpoint_relay;
        use super::super::worker_restart::{InstalledWorkerRestart, RESTART_FOR_CHECKPOINT};

        // Suspend can start a checkpoint worker and restart the harness on
        // rollback. Keep these futures out of its stack frame so it has room
        // to decode relay replies during checkpoint capture.
        Box::pin(async move {
            let reconnect = targets::reconnect_plan(&self.backend, session_id)?
                .commands
                .into_iter()
                .next()
                .context("reconnect plan is missing")?;
            let mut launch = controller.current_worker_launch_config(session_id, &self.backend)?;
            launch.run_mode = run_mode;
            launch.suspend_capture =
                (run_mode == WorkerRunMode::CheckpointOnly).then(|| self.capture_scope());
            let worker_root = self.root.as_str();
            let mut restart = InstalledWorkerRestart {
                backend: &self.backend,
                worker_root,
                reconnect: &reconnect,
                launch: Some(&launch),
                prepared: false,
                messages: &RESTART_FOR_CHECKPOINT,
                staged_aws_profile: None,
                swap_admission: None,
                discard_queued_prompts: None,
            };
            // Quiescence proved the original worker exited. If a worker has since
            // booted in the requested mode, observe it through the existing launch
            // owner, including when the daemon exited before recording readiness.
            let connection = if self.live_worker_mode(executor)? == Some(run_mode) {
                let pending = crate::database::load_worker_restart(session_id)?;
                executor.begin_resumable_work()?;
                let result = controller
                    .observe_started_worker(session_id, executor, restart)
                    .await;
                executor.end_resumable_work("reattach to the retained suspend worker")?;
                let connection = result?;
                if let Some(pending) = pending {
                    crate::database::finish_worker_restart(session_id, &pending.operation_id)?;
                }
                connection
            } else {
                ensure!(
                    !reattach_only,
                    "the retained suspend worker exited before reattachment"
                );
                const LABEL: &str = "start managed clone suspend worker";
                // A draining handoff refuses the swap. Step aside instead of
                // failing the accepted suspend: either the handoff closes the
                // gate, which interrupts this lifecycle for shutdown and the
                // next daemon resumes it from its durable intent, or the drain
                // lapses and the swap is admitted.
                restart.swap_admission = Some(loop {
                    match crate::upgrade::activity_unless_draining(LABEL) {
                        Ok(work) => break work,
                        Err(_) if crate::upgrade::is_draining() => {
                            executor.begin_resumable_work()?;
                            executor.end_resumable_work(LABEL)?;
                        }
                        Err(error) => return Err(error),
                    }
                });
                let result = controller
                    .restart_worker_with_installed_binary(session_id, executor, restart)
                    .await;
                // The restart releases admission only once the launch succeeds.
                // Release it here too (idempotent) so a failed launch never
                // waits for a draining handoff that is waiting for this hold.
                executor.begin_resumable_work()?;
                executor.end_resumable_work("record managed clone suspend worker readiness")?;
                result?
            };
            adopt_restarted_checkpoint_relay(admission, session_id, manager, connection).await
        })
    }
}

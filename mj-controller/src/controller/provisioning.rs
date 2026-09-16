//! Session provisioning, rollback, and worker-side Git bootstrap.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use mj_core::config::{TargetTemplate, atomic_write, data_dir};
use mj_core::state::{SessionState, State, TargetLocator};

use crate::targets::{
    self, CancellableProcessExecutor, CommandExecutor, CommandOutput, CommandSpec, ProvisionStage,
    ProvisionStageGuard,
};

use super::backend::{
    ContainerOverrides, backend_bundle, backend_locator, backend_target,
    configure_github_token_environment, controller_github_token, locator_after_provision,
    preflight_target, use_github_https_urls,
};
use super::git_cache;
use super::readiness::{connect_started_worker, wait_for_native_session_in_stage};
use super::worker_binary::{bridge_readiness_stage, start_worker, worker_probe_diagnosis};
use super::{Controller, execute_checked, now};

const INHERITED_GIT_SETTINGS: &[&str] = &[
    "diff.algorithm",
    "fetch.prune",
    "fetch.prunetags",
    "init.defaultbranch",
    "merge.conflictstyle",
    "pull.ff",
    "pull.rebase",
    "push.autosetupremote",
    "push.default",
    "rebase.autostash",
    "rerere.autoupdate",
    "rerere.enabled",
    "user.email",
    "user.name",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProvisioningFailureDisposition {
    /// A freshly registered session has no durable history to retain.
    Discard,
    /// Resume owns rollback to the archived record and checkpoint lineage.
    Preserve,
}

impl Controller {
    pub async fn provision_session_controlled_with_commit(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        grant_commit: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let github_token = controller_github_token();
        let repositories = self
            .provision_session_target_with_failure_disposition(
                session_id,
                executor,
                github_token.as_deref(),
                ProvisioningFailureDisposition::Discard,
            )
            .await?;
        let setup = execute_concurrent_lanes(
            || execute_repository_setup(&repositories, executor),
            || self.install_worker_payload(session_id, executor),
        );
        let result = match setup {
            Ok(((), (backend, worker_root))) => {
                self.connect_and_start_worker(session_id, executor, &backend, &worker_root, true)
                    .await
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(native_session_id) => {
                if let Err(error) = grant_commit() {
                    return Err(self.rollback_failed_new_session(session_id, error, executor)?);
                }
                self.mark_worker_connected(session_id, native_session_id)
            }
            Err(error) => Err(self.rollback_failed_new_session(session_id, error, executor)?),
        }
    }

    /// Start a child worker inside an already-provisioned parent target.
    /// Repository, target, and mount setup belong exclusively to the parent.
    pub async fn provision_subagent_session_controlled(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        // Placement failures must reach the same failure arm as startup
        // failures; otherwise the child record stays `Provisioning` forever.
        let placement = self.worker_placement(session_id);
        let (result, placement) = match placement {
            Ok((backend, worker_root)) => {
                let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
                let prepared =
                    self.prepare_worker_files(session_id, &backend, &worker_root, syncing);
                let result = match prepared {
                    Ok(()) => {
                        self.connect_and_start_worker(
                            session_id,
                            executor,
                            &backend,
                            &worker_root,
                            false,
                        )
                        .await
                    }
                    Err(error) => Err(error),
                };
                (result, Some((backend, worker_root)))
            }
            Err(error) => (Err(error), None),
        };
        match result {
            Ok(native_session_id) => self.mark_worker_connected(session_id, native_session_id),
            Err(error) => {
                // Without placement there is no worker to stop.
                if let Some((backend, worker_root)) = placement
                    && let Err(stop_error) =
                        super::worker_binary::stop_worker(executor, &backend, &worker_root)
                {
                    tracing::warn!(
                        session_id,
                        error = format!("{stop_error:#}"),
                        "failed sub-agent worker could not be stopped cleanly"
                    );
                }
                tracing::warn!(
                    session_id,
                    error = format!("{error:#}"),
                    "sub-agent startup failed"
                );
                let record = self
                    .state
                    .sessions
                    .get_mut(session_id)
                    .context("failed sub-agent session disappeared")?;
                record.state = SessionState::Error;
                record.updated_at = super::now();
                record.last_error = Some(format!("sub-agent startup failed: {error:#}"));
                crate::database::save_lifecycle_session(record)?;
                Err(error)
            }
        }
    }

    fn rollback_failed_new_session(
        &mut self,
        session_id: &str,
        error: anyhow::Error,
        executor: &impl CommandExecutor,
    ) -> Result<anyhow::Error> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        let target_cleanup = match session.target.as_ref() {
            Some(locator) => (|| -> Result<()> {
                let backend = backend_locator(locator, &session, &self.config)?;
                targets::close_plan(&backend, session_id)?
                    // Rollback must remain possible after the foreground
                    // operation's cancellation token has been set.
                    .execute(&CancellableProcessExecutor::with_timeout(
                        Duration::from_secs(15),
                    ))
                    .map(|_| ())
            })(),
            None => Ok(()),
        };
        let worktree_cleanup =
            self.cleanup_new_session_worktree_after_failure(session_id, executor);
        let cleanup_error = [target_cleanup, worktree_cleanup]
            .into_iter()
            .filter_map(Result::err)
            .map(|error| format!("{error:#}"))
            .collect::<Vec<_>>()
            .join("; ");
        if !cleanup_error.is_empty() {
            tracing::warn!(
                session_id,
                error = %cleanup_error,
                "new-session rollback cleanup reported failures"
            );
        }
        let original = note_new_session_launch_failure(session_id, &error);
        let failure = apply_failed_new_session_rollback(
            &mut self.state,
            session_id,
            &original,
            (!cleanup_error.is_empty()).then_some(cleanup_error),
        );
        self.persist_session_state(session_id)?;
        Ok(failure)
    }

    pub(super) async fn provision_session_with_failure_disposition(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        github_token: Option<&str>,
        failure_disposition: ProvisioningFailureDisposition,
    ) -> Result<()> {
        let repositories = self
            .provision_session_target_with_failure_disposition(
                session_id,
                executor,
                github_token,
                failure_disposition,
            )
            .await?;
        match execute_repository_setup(&repositories, executor) {
            Ok(()) => Ok(()),
            Err(error) if failure_disposition == ProvisioningFailureDisposition::Discard => {
                Err(self.rollback_failed_new_session(session_id, error, executor)?)
            }
            Err(error) => Err(error),
        }
    }

    async fn provision_session_target_with_failure_disposition(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        github_token: Option<&str>,
        failure_disposition: ProvisioningFailureDisposition,
    ) -> Result<targets::CommandPlan> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        if session.state != SessionState::Provisioning {
            bail!("session {session_id} is not provisioning");
        }
        let preparation = (|| {
            let template = self
                .config
                .targets
                .get(&session.target_template_id)
                .context("target template disappeared during provisioning")?;
            let profile = self
                .config
                .profiles
                .get(&session.last_profile)
                .context("harness profile disappeared during provisioning")?;
            super::worker_binary::preflight_harness(template, profile, executor)?;
            self.prepare_managed_raw_worktree(session_id, executor)
        })();
        let created_worktree = match preparation {
            Ok(created) => created,
            Err(error) if failure_disposition == ProvisioningFailureDisposition::Discard => {
                return Err(self.fail_new_session_with_cleanup(session_id, error, executor)?);
            }
            Err(error) => return Err(error),
        };
        let session = self
            .state
            .sessions
            .get(session_id)
            .expect("session retained after managed worktree preparation")
            .clone();
        // Keep planning, preflight, creation, and locator discovery in one
        // result so the caller's failure disposition applies to every error.
        let result = (|| {
            let template = self
                .config
                .targets
                .get(&session.target_template_id)
                .context("target template disappeared during provisioning")?;
            if matches!(template, TargetTemplate::AwsEc2 { .. }) {
                for resource in &session.additional_mounts {
                    ensure!(
                        resource.source.is_dir(),
                        "attached resource source is not a directory: {}",
                        resource.source.display()
                    );
                }
            }
            let mut target = backend_target(
                template,
                session.resource_allocation.as_ref(),
                ContainerOverrides::for_session(&session),
            )?;
            let mut runtime_mounts = if matches!(target, targets::TargetTemplate::AwsEc2(_)) {
                Vec::new()
            } else {
                session.additional_mounts.clone()
            };
            // The mounts this container runs with, not the ones the session
            // stores: a forced downgrade belongs to the host the container
            // lands on, so it is decided here every time and never written
            // over the user's choice.
            for notice in enforce_overlay_capable_mounts(&target, &mut runtime_mounts, executor) {
                executor.notify_notice(&notice);
            }
            // The image's user is a property of the host's copy of the image,
            // so it is read here, once per image per daemon, and handed to the
            // plan rather than stored on the session.
            let image_user = podman_image_user(&target, executor);
            let mut bundle = if session.project_directory.is_some() {
                None
            } else if failure_disposition == ProvisioningFailureDisposition::Preserve {
                Some(super::network_git::checkpoint_bundle(&session)?)
            } else {
                Some(backend_bundle(
                    self.config
                        .bundles
                        .get(&session.bundle_id)
                        .context("session bundle is missing")?,
                    executor,
                )?)
            };
            let container_github_token =
                github_token.filter(|_| configure_github_token_environment(&mut target));
            if container_github_token.is_some()
                && let Some(bundle) = bundle.as_mut()
            {
                use_github_https_urls(bundle);
            }
            preflight_target(template, executor)?;
            let prepared_cache = bundle.as_mut().and_then(|bundle| {
                git_cache::prepare(
                    &target,
                    session_id,
                    bundle,
                    &mut runtime_mounts,
                    container_github_token,
                    executor,
                )
            });
            // Mounts are fixed when the container is created, so the build
            // cache is decided here, before the provisioning plan is built.
            let build_cache = super::mbx::prepare(
                &target,
                &self.config.build_cache,
                &session,
                bundle.as_ref(),
                prepared_cache.as_ref(),
                &mut runtime_mounts,
                executor,
            );
            let provision = if let Some(project_directory) = &session.project_directory {
                targets::provision_bare_project_plan(
                    &target,
                    session_id,
                    &project_directory.to_string_lossy(),
                )
            } else {
                bundle
                    .as_ref()
                    .context("project bundle disappeared during provisioning")
                    .and_then(|bundle| {
                        targets::provision_plan(
                            &target,
                            session_id,
                            bundle,
                            &runtime_mounts,
                            image_user,
                            session.container_workspace.as_deref(),
                        )
                    })
            };
            let mut provision = match provision {
                Ok(provision) => provision,
                Err(error) => {
                    if let Some(cache) = &prepared_cache {
                        let _ = cache.cleanup(executor);
                    }
                    return Err(error);
                }
            };
            if let Some(token) = container_github_token
                && let Err(error) =
                    provision.provide_target_environment_secret(&target, "GH_TOKEN", token)
            {
                if let Some(cache) = &prepared_cache {
                    let _ = cache.cleanup(executor);
                }
                return Err(error);
            }

            let started = Instant::now();
            let result =
                provision_target_creation(&provision, &target, session_id, executor, |outputs| {
                    locator_after_provision(
                        template,
                        &target,
                        session_id,
                        outputs.first(),
                        executor,
                    )
                })
                .map(|(locator, remainder)| (locator, remainder, bundle, build_cache));
            if result.is_err()
                && let Some(cache) = &prepared_cache
            {
                if let Some(locator) = provisioned_locator(&target, session_id, None) {
                    let _ = targets::close_plan(&locator, session_id)
                        .and_then(|plan| plan.execute(executor).map(|_| ()));
                } else {
                    let _ = cache.cleanup(executor);
                }
            }
            tracing::debug!(
                session_id,
                elapsed_ms = started.elapsed().as_millis(),
                "provisioning plan execution completed"
            );
            result
        })();
        let result = match result {
            Err(error)
                if created_worktree
                    && failure_disposition == ProvisioningFailureDisposition::Discard =>
            {
                return Err(self.fail_new_session_with_cleanup(session_id, error, executor)?);
            }
            Err(error) if failure_disposition == ProvisioningFailureDisposition::Preserve => {
                Err(error)
            }
            Err(error) => {
                // This arm (no managed worktree to unwind) is the one a
                // provisioning failure such as a dropped target connection
                // hits; record the diagnostic and the session-id log here too.
                let detail = note_new_session_launch_failure(session_id, &error);
                {
                    let record = self.state.sessions.get_mut(session_id).unwrap();
                    record.state = SessionState::Error;
                    record.target = None;
                    record.updated_at = super::now();
                    record.last_error = Some(format!("session provisioning failed: {detail}"));
                }
                return match self.persist_session_state(session_id) {
                    Ok(()) => Err(error),
                    Err(persistence_error) => Err(error.context(format!(
                        "persist removal of failed provisioning session {session_id}: {persistence_error:#}"
                    ))),
                };
            }
            Ok((locator, remainder, bundle, build_cache)) => {
                apply_new_session_provisioning_result(&mut self.state, session_id, Ok(locator))?;
                self.state
                    .sessions
                    .get_mut(session_id)
                    .expect("session retained after provisioning")
                    .build_cache = build_cache;
                let session = &self.state.sessions[session_id];
                let backend = backend_locator(
                    session
                        .target
                        .as_ref()
                        .context("provisioned target disappeared")?,
                    session,
                    &self.config,
                )?;
                if matches!(backend, targets::TargetLocator::AwsEc2 { .. }) {
                    targets::provision_on_locator_plan(
                        &backend,
                        session_id,
                        bundle
                            .as_ref()
                            .context("AWS provisioning requires a project bundle")?,
                    )
                } else {
                    Ok(remainder)
                }
            }
        };
        let result = match result {
            Err(error) if failure_disposition == ProvisioningFailureDisposition::Discard => {
                return Err(self.rollback_failed_new_session(session_id, error, executor)?);
            }
            result => result,
        };
        if result.is_ok()
            && let Some(session) = self.state.sessions.get(session_id)
            && let Some(directory) = session
                .managed_worktree
                .as_ref()
                .map(|worktree| worktree.source_project_directory.clone())
                .or_else(|| session.project_directory.clone())
            && let Some(template) = self.config.targets.get(&session.target_template_id)
        {
            let host = match template {
                TargetTemplate::LocalBare => Some("local"),
                TargetTemplate::SshBare { ssh, .. } => Some(ssh.host.as_str()),
                _ => None,
            };
            if let Some(host) = host {
                self.state.remember_project_directory(host, &directory);
                crate::database::remember_project_directory(host, &directory)?;
            }
        }
        self.persist_session_state(session_id)?;
        result
    }

    pub fn mark_worker_connected(
        &mut self,
        session_id: &str,
        native_session_id: Option<String>,
    ) -> Result<()> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        if session.target.is_none() {
            bail!("session {session_id} has no provisioned target");
        }
        let updated_at = now();
        crate::database::mark_session_worker_connected(
            session_id,
            native_session_id.as_deref(),
            &updated_at,
        )?;
        let session = self
            .state
            .sessions
            .get_mut(session_id)
            .expect("session disappeared after its worker connection was saved");
        session.state = SessionState::Running;
        if native_session_id.is_some() {
            session.native_session_id = native_session_id;
        }
        session.updated_at = updated_at;
        session.last_error = None;
        Ok(())
    }

    fn install_worker_payload(
        &self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Result<(targets::TargetLocator, String)> {
        // Worker/profile installation is independent of repository cloning.
        let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
        let (backend, worker_root) = self.worker_placement(session_id)?;
        self.prepare_worker_files(session_id, &backend, &worker_root, syncing)?;
        install_attached_resources(&self.state, session_id, &backend, &worker_root, syncing)?;
        Ok((backend, worker_root))
    }

    async fn connect_and_start_worker(
        &self,
        session_id: &str,
        executor: &impl CommandExecutor,
        backend: &targets::TargetLocator,
        worker_root: &str,
        initialize_workspace: bool,
    ) -> Result<Option<String>> {
        let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
        if initialize_workspace {
            install_inherited_git_settings(executor, backend, session_id)?;
            self.initialize_network_workspaces(session_id, backend, syncing)?;
        }
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let profile = self
            .config
            .profiles
            .get(&session.last_profile)
            .with_context(|| format!("unknown profile {}", session.last_profile))?;
        let readiness_stage = bridge_readiness_stage(profile);
        let reconnect = &targets::reconnect_plan(backend, session_id)?.commands[0];
        let readiness = async {
            let mut relay = {
                let _starting = ProvisionStageGuard::new(executor, ProvisionStage::Starting);
                start_worker(executor, backend, worker_root)?;
                connect_started_worker(reconnect, session_id, executor, backend, worker_root)
                    .await?
            };
            let native_session_id =
                wait_for_native_session_in_stage(&mut relay, executor, readiness_stage).await?;
            Ok(Some(native_session_id))
        }
        .await;
        match readiness {
            Ok(native_session_id) => Ok(native_session_id),
            Err(error) => Err(worker_probe_diagnosis(
                executor,
                backend,
                worker_root,
                error,
            )),
        }
    }
}

const MAX_LAUNCH_DIAGNOSTIC_BYTES: usize = 64 * 1024;

const RETAINED_LAUNCH_DIAGNOSTICS: usize = 20;

/// Record a failed new-session launch consistently across every failure arm:
/// log the failure with the session id (so `logs/mj-*.log` names the session)
/// and save the local diagnostic file. Returns the underlying error chain
/// annotated with the diagnostic path, which the caller stores in `last_error`
/// so the reason travels to `mj sessions`, `mj wait`, and `mj events`.
pub(super) fn note_new_session_launch_failure(session_id: &str, error: &anyhow::Error) -> String {
    note_new_session_launch_failure_in(&data_dir().join("diagnostics"), session_id, error)
}

fn note_new_session_launch_failure_in(
    directory: &Path,
    session_id: &str,
    error: &anyhow::Error,
) -> String {
    let original = format!("{error:#}");
    tracing::warn!(session_id, error = %original, "session launch failed");
    match persist_launch_failure_to(directory, session_id, &original) {
        Ok(path) => format!("{original}; full diagnostic saved to {}", path.display()),
        Err(save_error) => {
            format!("{original}; saving the local diagnostic failed: {save_error:#}")
        }
    }
}

fn persist_launch_failure_to(directory: &Path, session_id: &str, detail: &str) -> Result<PathBuf> {
    mj_core::config::validate_id("session", session_id)?;
    std::fs::create_dir_all(directory).with_context(|| {
        format!(
            "create launch diagnostics directory {}",
            directory.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = directory.join(format!("{session_id}-launch-error.txt"));
    let detail = bounded_launch_diagnostic(detail);
    let body = format!(
        "Hel session launch failure\nsession: {session_id}\nat: {}\n\n{detail}\n",
        now()
    );
    atomic_write(&path, body.as_bytes())?;
    prune_launch_diagnostics(directory)?;
    Ok(path)
}

fn bounded_launch_diagnostic(detail: &str) -> String {
    if detail.len() <= MAX_LAUNCH_DIAGNOSTIC_BYTES {
        return detail.to_owned();
    }
    let mut head_end = MAX_LAUNCH_DIAGNOSTIC_BYTES / 4;
    while !detail.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let tail_bytes = MAX_LAUNCH_DIAGNOSTIC_BYTES - head_end;
    let mut tail_start = detail.len() - tail_bytes;
    while !detail.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!(
        "{}\n\n[... launch diagnostic truncated ...]\n\n{}",
        &detail[..head_end],
        &detail[tail_start..]
    )
}

fn prune_launch_diagnostics(directory: &Path) -> Result<()> {
    let mut diagnostics = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if !entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with("-launch-error.txt"))
        {
            continue;
        }
        diagnostics.push((entry.metadata()?.modified()?, entry.path()));
    }
    diagnostics.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, path) in diagnostics.into_iter().skip(RETAINED_LAUNCH_DIAGNOSTICS) {
        std::fs::remove_file(&path)
            .with_context(|| format!("prune old launch diagnostic {}", path.display()))?;
    }
    Ok(())
}

fn apply_new_session_provisioning_result(
    state: &mut State,
    session_id: &str,
    result: Result<TargetLocator>,
) -> Result<()> {
    match result {
        Ok(locator) => {
            let record = state.sessions.get_mut(session_id).unwrap();
            record.target = Some(locator);
            // Provisioning has completed, but Running is reserved for a
            // successful worker handshake.
            record.state = SessionState::Disconnected;
            record.updated_at = now();
            record.last_error = None;
            Ok(())
        }
        Err(error) => {
            let record = state.sessions.get_mut(session_id).unwrap();
            record.state = SessionState::Error;
            record.target = None;
            record.updated_at = now();
            record.last_error = Some(format!("session provisioning failed: {error:#}"));
            Err(error)
        }
    }
}

pub(super) fn apply_failed_new_session_rollback(
    state: &mut State,
    session_id: &str,
    original_error: &str,
    cleanup_error: Option<String>,
) -> anyhow::Error {
    match cleanup_error {
        None => {
            let record = state.sessions.get_mut(session_id).unwrap();
            record.state = SessionState::Error;
            record.target = None;
            record.updated_at = now();
            record.last_error = Some(format!("worker bootstrap failed: {original_error}"));
            anyhow::anyhow!("{original_error}; partial target removed and failed session retained")
        }
        Some(cleanup_error) => {
            let failure = format!(
                "{original_error}; cleanup of the failed session target failed: {cleanup_error}"
            );
            let record = state.sessions.get_mut(session_id).unwrap();
            record.state = SessionState::Error;
            record.updated_at = now();
            record.last_error = Some(format!("worker bootstrap failed: {failure}"));
            anyhow::anyhow!(failure)
        }
    }
}

pub(super) fn install_attached_resources(
    state: &State,
    session_id: &str,
    backend: &targets::TargetLocator,
    worker_root: &str,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let targets::TargetLocator::AwsEc2 { .. } = backend else {
        return Ok(());
    };
    let session = state
        .sessions
        .get(session_id)
        .with_context(|| format!("unknown session {session_id}"))?;
    if session.additional_mounts.is_empty() {
        return Ok(());
    }
    for resource in &session.additional_mounts {
        let install = targets::command_on_locator(
            backend,
            session_id,
            vec![
                format!("{worker_root}/hel"),
                "worker".into(),
                "install-resource".into(),
                "--destination".into(),
                resource.destination.to_string_lossy().into_owned(),
            ],
            "stream attached resource",
        )?;
        mj_checkpoint::resources::stream_resource(&resource.source, |stream| {
            execute_checked_with_stdin(executor, &install, stream).map(|_| ())
        })
        .with_context(|| format!("stream attached resource {}", resource.source.display()))?;
    }
    Ok(())
}

/// Run two independent target setup lanes at the same time and wait for both.
/// The first lane's failure wins deterministically when both fail, and neither
/// lane is abandoned while it may still own a transfer or subprocess.
pub(super) fn execute_concurrent_lanes<A: Send, B: Send>(
    first: impl FnOnce() -> Result<A> + Send,
    second: impl FnOnce() -> Result<B> + Send,
) -> Result<(A, B)> {
    std::thread::scope(|scope| {
        let second = scope.spawn(second);
        let first = first();
        let second = second.join().unwrap_or_else(|panic| {
            Err(anyhow::anyhow!(
                "concurrent target lane panicked: {}",
                targets::command_thread_panic_message(panic.as_ref())
            ))
        });
        match (first, second) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(first), Ok(second)) => Ok((first, second)),
        }
    })
}

fn execute_repository_setup(
    plan: &targets::CommandPlan,
    executor: &(impl CommandExecutor + Sync),
) -> Result<()> {
    if plan.commands.is_empty() {
        return Ok(());
    }
    let _cloning = ProvisionStageGuard::new(executor, ProvisionStage::Cloning);
    plan.execute_concurrent(executor).map(|_| ())
}

/// Run a provisioning plan and discover the locator it produced, tearing the
/// target down again if anything after its creation fails.
///
/// Creation is the boundary that matters. A step that fails before the target
/// exists has left nothing behind; every failure after it — a later plan step
/// or locator discovery — owns a target no session record will point at.
#[cfg(test)]
fn provision_target(
    plan: &targets::CommandPlan,
    target: &targets::TargetTemplate,
    session_id: &str,
    executor: &(impl CommandExecutor + Sync),
    discover: impl FnOnce(&[CommandOutput]) -> Result<TargetLocator>,
) -> Result<TargetLocator> {
    let Some((creation, remainder)) = plan.split_at_target_creation() else {
        // Nothing this plan runs can leave a target behind.
        return discover(&plan.execute_concurrent(executor)?);
    };
    let mut outputs = creation.execute_concurrent(executor)?;
    let result = match remainder.execute_concurrent(executor) {
        Ok(rest) => {
            outputs.extend(rest);
            discover(&outputs)
        }
        Err(error) => Err(error),
    };
    result.map_err(|error| {
        match cleanup_failed_provision(target, session_id, outputs.first(), executor) {
            Some(note) => error.context(note),
            None => error,
        }
    })
}

/// Bring the target into existence and return the commands that populate its
/// repositories. The caller may overlap that remainder with worker/profile
/// installation once it has persisted the discovered locator.
fn provision_target_creation(
    plan: &targets::CommandPlan,
    target: &targets::TargetTemplate,
    session_id: &str,
    executor: &(impl CommandExecutor + Sync),
    discover: impl FnOnce(&[CommandOutput]) -> Result<TargetLocator>,
) -> Result<(TargetLocator, targets::CommandPlan)> {
    let Some((creation, remainder)) = plan.split_at_target_creation() else {
        // Nothing this plan runs can leave a target behind, so its commands
        // must still finish before the locator is usable.
        let outputs = plan.execute_concurrent(executor)?;
        return discover(&outputs).map(|locator| {
            (
                locator,
                targets::CommandPlan {
                    description: plan.description.clone(),
                    commands: Vec::new(),
                },
            )
        });
    };
    let outputs = creation.execute_concurrent(executor)?;
    discover(&outputs)
        .map(|locator| (locator, remainder))
        .map_err(|error| {
            match cleanup_failed_provision(target, session_id, outputs.first(), executor) {
                Some(note) => error.context(note),
                None => error,
            }
        })
}

/// Best-effort teardown of a target whose creation succeeded but whose
/// provisioning failed before a locator was recorded. Returns a note
/// describing what happened for inclusion in the session error.
///
/// The teardown is the session's own close plan, so a failed launch and an
/// ordinary close can never disagree about what removing a target means.
fn cleanup_failed_provision(
    target: &targets::TargetTemplate,
    session_id: &str,
    create_output: Option<&CommandOutput>,
    executor: &impl CommandExecutor,
) -> Option<String> {
    let locator = provisioned_locator(target, session_id, create_output)?;
    let leak = format!(
        "the resource may still exist; find it via its dev.mj.session={session_id} label/tag"
    );
    let plan = match targets::close_plan(&locator, session_id) {
        Ok(plan) => plan,
        Err(error) => {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "could not build provisioning cleanup plan"
            );
            return Some(format!("cleanup FAILED: {error:#}; {leak}"));
        }
    };
    let purpose = plan
        .commands
        .iter()
        .map(|command| command.purpose.clone())
        .collect::<Vec<_>>()
        .join("; ");
    let Err(error) = plan.execute(executor) else {
        return Some(format!("cleanup succeeded: {purpose}"));
    };
    match targets::cleanup_target_is_confirmed_absent(&locator, session_id, executor) {
        Ok(true) => Some(format!("cleanup succeeded: {purpose}")),
        Ok(false) => {
            tracing::warn!(
                session_id,
                error = format!("{error:#}"),
                "provisioning cleanup failed and the target may still exist"
            );
            Some(format!("cleanup FAILED ({purpose}): {error:#}; {leak}"))
        }
        Err(confirm_error) => {
            tracing::warn!(
                session_id,
                error = format!("{confirm_error:#}"),
                "could not confirm whether the failed provisioning target was removed"
            );
            Some(format!(
                "cleanup FAILED ({purpose}): {error:#}; checking whether it was removed also failed: {confirm_error:#}; {leak}"
            ))
        }
    }
}

/// The locator a provisioning plan's creating command brought into existence.
///
/// Every target but AWS is named before its plan runs; an EC2 instance
/// reports its own ID in the launch response.
fn provisioned_locator(
    target: &targets::TargetTemplate,
    session_id: &str,
    create_output: Option<&CommandOutput>,
) -> Option<targets::TargetLocator> {
    let container_id = || targets::resource_name(session_id).ok();
    Some(match target {
        // A bare project directory belongs to the user: provisioning creates
        // nothing that a failure could leak.
        targets::TargetTemplate::LocalBare => return None,
        targets::TargetTemplate::LocalPodman(container) => targets::TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: container_id()?,
            workspace_storage: targets::podman_workspace_locator(container, session_id).ok()?,
        },
        targets::TargetTemplate::LocalDocker(_) => targets::TargetLocator::LocalDocker {
            borrowed_from: None,
            container_id: container_id()?,
        },
        targets::TargetTemplate::AppleContainer(_) => targets::TargetLocator::AppleContainer {
            borrowed_from: None,
            container_id: container_id()?,
        },
        targets::TargetTemplate::SshPodman { ssh, container } => {
            targets::TargetLocator::SshPodman {
                borrowed_from: None,
                ssh: ssh.clone(),
                container_id: container_id()?,
                workspace_storage: targets::podman_workspace_locator(container, session_id).ok()?,
            }
        }
        targets::TargetTemplate::SshDocker { ssh, .. } => targets::TargetLocator::SshDocker {
            borrowed_from: None,
            ssh: ssh.clone(),
            container_id: container_id()?,
        },
        targets::TargetTemplate::SshBare { ssh, .. } => targets::TargetLocator::SshBare {
            ssh: ssh.clone(),
            workspace: targets::workspace_for(target, session_id).ok()?,
            worker_id: None,
        },
        targets::TargetTemplate::AwsEc2(aws) => targets::TargetLocator::AwsEc2 {
            profile: aws.profile.clone(),
            region: aws.region.clone(),
            instance_id: serde_json::from_slice::<serde_json::Value>(&create_output?.stdout)
                .ok()?
                .pointer("/Instances/0/InstanceId")?
                .as_str()?
                .to_owned(),
            ssh: aws.ssh.clone(),
            workspace: targets::workspace_for(target, session_id).ok()?,
        },
    })
}

/// Attach read-only whatever the selected container overlay cannot hold, and
/// say so.
///
/// The filesystem is probed on the host that runs the container, because that
/// is where the overlay would be built. A probe that cannot answer leaves the
/// overlay alone: a failed probe is no evidence of an unsupported filesystem,
/// and refusing to provision over one would cost the user their session.
///
/// Apple's `container` engine already mounts every extra directory read-only,
/// and EC2 copies the directory instead of mounting it, so neither is probed.
pub(super) fn enforce_overlay_capable_mounts(
    target: &targets::TargetTemplate,
    mounts: &mut [targets::AdditionalMount],
    executor: &impl CommandExecutor,
) -> Vec<String> {
    let ssh = match target {
        targets::TargetTemplate::LocalPodman(_) | targets::TargetTemplate::LocalDocker(_) => None,
        targets::TargetTemplate::SshPodman { ssh, .. }
        | targets::TargetTemplate::SshDocker { ssh, .. } => Some(ssh),
        _ => return Vec::new(),
    };
    let overlaid = mounts
        .iter()
        .filter(|mount| mount.access == targets::MountAccess::Cow)
        .map(|mount| mount.source.clone())
        .collect::<Vec<_>>();
    if overlaid.is_empty() {
        return Vec::new();
    }
    let filesystems = match targets::probe_filesystem_types(ssh, &overlaid, executor) {
        Ok(filesystems) => filesystems,
        Err(error) => {
            tracing::warn!(
                error = format!("{error:#}"),
                "could not probe attached-directory filesystems; preserving overlay mounts"
            );
            return vec![format!(
                "Could not read the filesystem under the attached directories, so they keep the \
                 copy-on-write overlay: {error:#}"
            )];
        }
    };
    let mut notices = Vec::new();
    for (mount, filesystem) in mounts
        .iter_mut()
        .filter(|mount| mount.access == targets::MountAccess::Cow)
        .zip(filesystems)
    {
        let Some(reason) = targets::overlay_unsupported_filesystem(&filesystem) else {
            continue;
        };
        mount.access = mount.access.without_overlay();
        notices.push(format!(
            "Mounted {} read-only: the overlay is unreliable on {filesystem} ({reason}).",
            mount.source.display()
        ));
    }
    notices
}

/// The image users already probed, keyed by container host and image
/// reference. An image's
/// configured user does not change under a fixed reference, and reading it
/// costs a container start, so each daemon asks a host once.
static IMAGE_USERS: std::sync::LazyLock<std::sync::Mutex<BTreeMap<String, targets::ImageUser>>> =
    std::sync::LazyLock::new(std::sync::Mutex::default);

/// The uid and gid a Podman session container maps onto the host user.
///
/// Only Podman is asked: Docker and Apple's `container` engine are left with
/// their own defaults. A probe that cannot answer is not a launch failure —
/// the container falls back to plain `--userns=keep-id`, which maps the
/// image's default user, and the user is told what happened.
pub(super) fn podman_image_user(
    target: &targets::TargetTemplate,
    executor: &impl CommandExecutor,
) -> Option<targets::ImageUser> {
    let (ssh, container) = match target {
        targets::TargetTemplate::LocalPodman(container) => (None, container),
        targets::TargetTemplate::SshPodman { ssh, container } => (Some(ssh), container),
        _ => return None,
    };
    let image = container.image.as_str();
    let key = format!(
        "{}|{image}",
        ssh.map_or("local", |ssh| ssh.destination.as_str())
    );
    if let Some(cached) = IMAGE_USERS.lock().expect("image user cache").get(&key) {
        return Some(*cached);
    }
    match targets::probe_image_user(ssh, container, executor) {
        Ok(user) => {
            IMAGE_USERS
                .lock()
                .expect("image user cache")
                .insert(key, user);
            Some(user)
        }
        Err(error) => {
            tracing::warn!(
                image,
                error = format!("{error:#}"),
                "could not read the container image user; keeping Podman's default user mapping"
            );
            executor.notify_notice(&format!(
                "Could not read the user of image {image}, so the container runs with Podman's \
                 default user mapping and may not be able to write to an attached directory: \
                 {error:#}"
            ));
            None
        }
    }
}

/// Reports every command an installer issues as one launch stage, so progress
/// stays accurate without threading the stage through each `CommandSpec`.
/// A command that already names a stage keeps it.
pub(super) struct StagedExecutor<'a, E: CommandExecutor> {
    inner: &'a E,
    stage: ProvisionStage,
    _guard: ProvisionStageGuard<'a, E>,
}

impl<'a, E: CommandExecutor> StagedExecutor<'a, E> {
    pub(crate) fn new(inner: &'a E, stage: ProvisionStage) -> Self {
        Self {
            inner,
            stage,
            _guard: ProvisionStageGuard::new(inner, stage),
        }
    }

    fn staged(&self, command: &CommandSpec) -> CommandSpec {
        if command.stage.is_some() {
            return command.clone();
        }
        command.clone().stage(self.stage)
    }
}

impl<E: CommandExecutor> CommandExecutor for StagedExecutor<'_, E> {
    fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
        self.inner.execute(&self.staged(command))
    }

    fn cancellation_requested(&self) -> bool {
        self.inner.cancellation_requested()
    }

    fn stage_started(&self, stage: ProvisionStage) {
        self.inner.stage_started(stage);
    }

    fn stage_finished(&self, stage: ProvisionStage) {
        self.inner.stage_finished(stage);
    }

    fn notify_notice(&self, notice: &str) {
        self.inner.notify_notice(notice);
    }

    fn execute_with_stdin(
        &self,
        command: &CommandSpec,
        input: &mut (dyn std::io::Read + Send),
    ) -> Result<CommandOutput> {
        self.inner.execute_with_stdin(&self.staged(command), input)
    }
}

fn execute_checked_with_stdin(
    executor: &impl CommandExecutor,
    command: &CommandSpec,
    input: &mut (dyn std::io::Read + Send),
) -> Result<CommandOutput> {
    let output = executor.execute_with_stdin(command, input)?;
    if output.status != 0 {
        bail!(
            "{} failed with status {}: {}",
            command.purpose,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output)
}

pub(super) fn install_inherited_git_settings(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
) -> Result<()> {
    let settings = if inherits_controller_git_settings(locator) {
        controller_git_settings()?
    } else {
        BTreeMap::new()
    };
    for command in inherited_git_setting_commands(locator, session_id, settings)? {
        execute_checked(executor, command)?;
    }
    Ok(())
}

fn inherits_controller_git_settings(locator: &targets::TargetLocator) -> bool {
    !matches!(
        locator,
        targets::TargetLocator::LocalBare { .. } | targets::TargetLocator::SshBare { .. }
    )
}

fn inherited_git_setting_commands(
    locator: &targets::TargetLocator,
    session_id: &str,
    settings: BTreeMap<String, String>,
) -> Result<Vec<CommandSpec>> {
    if matches!(locator, targets::TargetLocator::SshBare { .. }) {
        return Ok(Vec::new());
    }
    settings
        .into_iter()
        .map(|(key, value)| {
            targets::command_on_locator(
                locator,
                session_id,
                vec![
                    "git".into(),
                    "config".into(),
                    "--global".into(),
                    "--replace-all".into(),
                    "--".into(),
                    key.clone(),
                    value,
                ],
                format!("inherit Git setting {key}"),
            )
        })
        .collect()
}

fn controller_git_settings() -> Result<BTreeMap<String, String>> {
    let output = match Command::new("git")
        .args(["config", "--global", "--includes", "--null", "--list"])
        .stdin(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error).context("read controller Git configuration"),
    };
    if !output.status.success() {
        bail!(
            "read controller Git configuration failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_inherited_git_settings(&output.stdout)
}

fn parse_inherited_git_settings(output: &[u8]) -> Result<BTreeMap<String, String>> {
    let mut settings = BTreeMap::new();
    for entry in output
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let entry = std::str::from_utf8(entry).context("decode controller Git configuration")?;
        let (key, value) = entry
            .split_once('\n')
            .with_context(|| format!("controller Git returned malformed entry {entry:?}"))?;
        let key = key.to_ascii_lowercase();
        if INHERITED_GIT_SETTINGS.contains(&key.as_str()) {
            settings.insert(key, value.to_owned());
        }
    }
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use std::sync::Mutex;

    use mj_core::config::{
        Config, ContainerTemplate as ConfigContainer, HarnessKind, HarnessProfile, ProjectBundle,
        ProjectRepository, SshConnection,
    };
    use mj_core::state::{SessionRecord, SessionState, State, TargetLocator};

    use crate::targets::{self, AdditionalMount, ContainerTemplate, ProjectBundleSpec, SshTarget};

    use crate::controller::SessionLaunchOptions;

    use super::*;

    /// Answers the filesystem probe, and records every notice provisioning
    /// reported while it ran.
    struct ProbeExecutor {
        answer: std::result::Result<&'static str, &'static str>,
        notices: Mutex<Vec<String>>,
    }

    impl ProbeExecutor {
        fn answering(answer: &'static str) -> Self {
            Self {
                answer: Ok(answer),
                notices: Mutex::new(Vec::new()),
            }
        }

        fn failing(stderr: &'static str) -> Self {
            Self {
                answer: Err(stderr),
                notices: Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandExecutor for ProbeExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            assert_eq!(command.program, "stat", "only the probe may run here");
            Ok(match self.answer {
                Ok(filesystem) => CommandOutput {
                    status: 0,
                    stdout: format!("{filesystem}\n").into_bytes(),
                    stderr: Vec::new(),
                },
                Err(stderr) => CommandOutput {
                    status: 1,
                    stdout: Vec::new(),
                    stderr: stderr.as_bytes().to_vec(),
                },
            })
        }

        fn notify_notice(&self, notice: &str) {
            self.notices.lock().unwrap().push(notice.to_owned());
        }
    }

    fn podman_target() -> targets::TargetTemplate {
        targets::TargetTemplate::LocalPodman(ContainerTemplate {
            build_cache: None,
            image: "ubuntu:24.04".into(),
            pull_policy: Default::default(),
            extra_run_args: Vec::new(),
            workspace_storage: Default::default(),
        })
    }

    fn probe_bundle() -> ProjectBundleSpec {
        ProjectBundleSpec {
            primary: "app".into(),
            repositories: vec![crate::targets::RepositorySpec {
                url: Some("https://github.com/example/app.git".into()),
                push_urls: Vec::new(),
                destination: "app".into(),
                git_ref: None,
                reference: None,
            }],
        }
    }

    fn ssh_docker_registration_config() -> Config {
        let mut config = Config::default();
        config.profiles.insert(
            "codex".into(),
            HarnessProfile {
                enabled: true,
                kind: HarnessKind::Codex,
                home: PathBuf::from("/home/dev/.codex"),
                environment: BTreeMap::new(),
                context_window_bytes: None,
                guardian_review_model: None,
            },
        );
        config.bundles.insert(
            "project".into(),
            ProjectBundle {
                primary_repo: "project".into(),
                repositories: vec![ProjectRepository {
                    id: "project".into(),
                    github: Some("owner/project".into()),
                    local: None,
                    destination: PathBuf::from("project"),
                    git_ref: None,
                }],
            },
        );
        config.targets.insert(
            "docker".into(),
            TargetTemplate::SshDocker {
                ssh: SshConnection {
                    host: "builder".into(),
                    user: Some("agent".into()),
                    identity_file: None,
                    extra_args: Vec::new(),
                },
                container: ConfigContainer {
                    build_cache: None,
                    image: "failimage:never".into(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: BTreeMap::new(),
                    workspace_storage: Default::default(),
                },
            },
        );
        config
    }

    #[test]
    fn the_build_cache_is_mounted_read_write_at_its_host_path() {
        let mut mounts = Vec::new();
        super::super::mbx::attach_mounts_for_tests(
            &mj_core::state::SessionBuildCache {
                host: "local-podman".into(),
                directory: PathBuf::from("/mnt/fast/mbx-cache"),
                max_size: None,
                target_root: Some(PathBuf::from("/mnt/fast/mbx-targets")),
            },
            &mut mounts,
        );

        let plan = targets::provision_plan(
            &podman_target(),
            "0123456789abcdef0123456789abcdef",
            &probe_bundle(),
            &mounts,
            None,
            None,
        )
        .unwrap();

        for directory in ["/mnt/fast/mbx-cache", "/mnt/fast/mbx-targets"] {
            let expected = format!("{directory}:{directory}:rw");
            assert!(
                plan.commands[0]
                    .args
                    .windows(2)
                    .any(|args| args == ["--volume", expected.as_str()]),
                "{:?}",
                plan.commands[0].args
            );
        }
    }

    #[test]
    fn a_source_that_cannot_overlay_is_mounted_read_only_and_reported() {
        let executor = ProbeExecutor::answering("nfs");
        let mut mounts = vec![AdditionalMount {
            source: PathBuf::from("/nfs/share"),
            destination: PathBuf::from("/mnt/share"),
            access: crate::targets::MountAccess::Cow,
        }];

        let notices = enforce_overlay_capable_mounts(&podman_target(), &mut mounts, &executor);

        assert_eq!(mounts[0].access, targets::MountAccess::Ro);
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0]
                .contains("Mounted /nfs/share read-only: the overlay is unreliable on nfs (network filesystem)"),
            "{notices:?}"
        );
        let plan = targets::provision_plan(
            &podman_target(),
            "0123456789abcdef0123456789abcdef",
            &probe_bundle(),
            &mounts,
            None,
            None,
        )
        .unwrap();
        assert!(
            plan.commands[0]
                .args
                .windows(2)
                .any(|args| args == ["--volume", "/nfs/share:/mnt/share:ro"]),
            "{:?}",
            plan.commands[0].args
        );
    }

    #[test]
    fn a_probe_that_cannot_answer_keeps_the_overlay_and_says_so() {
        let executor = ProbeExecutor::failing("stat: cannot read file system information");
        let mut mounts = vec![AdditionalMount {
            source: PathBuf::from("/host/cache"),
            destination: PathBuf::from("/mnt/cache"),
            access: crate::targets::MountAccess::Cow,
        }];

        let notices = enforce_overlay_capable_mounts(&podman_target(), &mut mounts, &executor);

        assert_eq!(mounts[0].access, targets::MountAccess::Cow);
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].contains("keep the copy-on-write overlay")
                && notices[0].contains("cannot read file system information"),
            "{notices:?}"
        );
        let plan = targets::provision_plan(
            &podman_target(),
            "0123456789abcdef0123456789abcdef",
            &probe_bundle(),
            &mounts,
            None,
            None,
        )
        .unwrap();
        assert!(
            plan.commands[0]
                .args
                .windows(2)
                .any(|args| args == ["--volume", "/host/cache:/mnt/cache:O"]),
            "{:?}",
            plan.commands[0].args
        );
    }

    /// Answers the image-user probe, and records every notice and command.
    struct ImageUserExecutor {
        answer: std::result::Result<&'static str, &'static str>,
        seen: Mutex<Vec<CommandSpec>>,
        notices: Mutex<Vec<String>>,
    }

    impl ImageUserExecutor {
        fn new(answer: std::result::Result<&'static str, &'static str>) -> Self {
            Self {
                answer,
                seen: Mutex::new(Vec::new()),
                notices: Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandExecutor for ImageUserExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            self.seen.lock().unwrap().push(command.clone());
            Ok(match self.answer {
                Ok(ids) => CommandOutput {
                    status: 0,
                    stdout: ids.as_bytes().to_vec(),
                    stderr: Vec::new(),
                },
                Err(stderr) => CommandOutput {
                    status: 125,
                    stdout: Vec::new(),
                    stderr: stderr.as_bytes().to_vec(),
                },
            })
        }

        fn notify_notice(&self, notice: &str) {
            self.notices.lock().unwrap().push(notice.to_owned());
        }
    }

    fn podman_target_with_image(image: &str) -> targets::TargetTemplate {
        targets::TargetTemplate::LocalPodman(ContainerTemplate {
            build_cache: None,
            image: image.into(),
            pull_policy: Default::default(),
            extra_run_args: Vec::new(),
            workspace_storage: Default::default(),
        })
    }

    #[test]
    fn the_image_user_is_probed_once_per_image_and_reaches_the_run_arguments() {
        let target = podman_target_with_image("ghcr.io/example/probed-once:1");
        let executor = ImageUserExecutor::new(Ok("1000\n1000\n"));

        let image_user = podman_image_user(&target, &executor);

        assert_eq!(
            image_user,
            Some(targets::ImageUser {
                uid: 1000,
                gid: 1000
            })
        );
        assert!(executor.notices.lock().unwrap().is_empty());
        // The answer is cached for the daemon's lifetime.
        assert_eq!(podman_image_user(&target, &executor), image_user);
        assert_eq!(executor.seen.lock().unwrap().len(), 1);

        let plan = targets::provision_plan(
            &target,
            "0123456789abcdef0123456789abcdef",
            &probe_bundle(),
            &[],
            image_user,
            None,
        )
        .unwrap();
        assert!(
            plan.commands[0]
                .args
                .contains(&"--userns=keep-id:uid=1000,gid=1000".to_owned()),
            "{:?}",
            plan.commands[0].args
        );
    }

    #[test]
    fn an_image_user_probe_that_fails_keeps_podmans_default_mapping_and_says_so() {
        let target = podman_target_with_image("ghcr.io/example/unreadable-user:1");
        let executor = ImageUserExecutor::new(Err("image not known"));

        let image_user = podman_image_user(&target, &executor);

        assert_eq!(image_user, None);
        let notices = executor.notices.lock().unwrap().clone();
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].contains("ghcr.io/example/unreadable-user:1")
                && notices[0].contains("image not known"),
            "{notices:?}"
        );
        // A failed probe is never fatal, and never cached.
        assert_eq!(podman_image_user(&target, &executor), None);
        assert_eq!(executor.seen.lock().unwrap().len(), 2);

        let plan = targets::provision_plan(
            &target,
            "0123456789abcdef0123456789abcdef",
            &probe_bundle(),
            &[],
            image_user,
            None,
        )
        .unwrap();
        // Plain `keep-id` would demote a root image to uid 1000, so a
        // container whose image user is unknown keeps Podman's own mapping.
        assert!(
            !plan.commands[0]
                .args
                .iter()
                .any(|argument| argument.starts_with("--userns")),
            "{:?}",
            plan.commands[0].args
        );
    }

    /// Docker and Apple's engine keep their own user namespaces, so nothing is
    /// probed for them.
    #[test]
    fn only_podman_targets_probe_the_image_user() {
        struct UnusedExecutor;

        impl CommandExecutor for UnusedExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                panic!("this target must not probe: {}", command.program)
            }
        }

        for target in [
            targets::TargetTemplate::LocalDocker(ContainerTemplate {
                build_cache: None,
                image: "ubuntu:24.04".into(),
                pull_policy: Default::default(),
                extra_run_args: Vec::new(),
                workspace_storage: Default::default(),
            }),
            targets::TargetTemplate::AppleContainer(ContainerTemplate {
                build_cache: None,
                image: "ubuntu:24.04".into(),
                pull_policy: Default::default(),
                extra_run_args: Vec::new(),
                workspace_storage: Default::default(),
            }),
        ] {
            assert_eq!(podman_image_user(&target, &UnusedExecutor), None);
        }
    }

    #[test]
    fn engines_without_an_overlay_to_lose_are_never_probed() {
        struct UnusedExecutor;

        impl CommandExecutor for UnusedExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                panic!("this target must not probe: {}", command.program)
            }
        }

        let mut mounts = vec![AdditionalMount {
            source: PathBuf::from("/host/cache"),
            destination: PathBuf::from("/mnt/cache"),
            access: crate::targets::MountAccess::Cow,
        }];
        for target in [
            targets::TargetTemplate::AppleContainer(ContainerTemplate {
                build_cache: None,
                image: "ubuntu:24.04".into(),
                pull_policy: Default::default(),
                extra_run_args: Vec::new(),
                workspace_storage: Default::default(),
            }),
            targets::TargetTemplate::AwsEc2(targets::AwsTemplate {
                profile: "default".into(),
                region: "us-east-1".into(),
                launch_template: "lt-0123456789abcdef0".into(),
                launch_template_version: None,
                instance_type: None,
                ssh: SshTarget {
                    destination: "ubuntu@example.test".into(),
                    ssh_args: Vec::new(),
                },
            }),
        ] {
            assert!(
                enforce_overlay_capable_mounts(&target, &mut mounts, &UnusedExecutor).is_empty()
            );
            assert_eq!(mounts[0].access, targets::MountAccess::Cow);
        }
    }

    /// A mount the user already marked read-only has no overlay to protect, so
    /// the probe never has to reach a host that may not answer.
    #[test]
    fn mounts_without_an_overlay_are_not_probed() {
        struct UnusedExecutor;

        impl CommandExecutor for UnusedExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                panic!(
                    "a mount without an overlay must not probe: {}",
                    command.program
                )
            }
        }

        let mut mounts = vec![
            AdditionalMount {
                source: PathBuf::from("/host/cache"),
                destination: PathBuf::from("/mnt/cache"),
                access: crate::targets::MountAccess::Ro,
            },
            AdditionalMount {
                source: PathBuf::from("/host/build-cache"),
                destination: PathBuf::from("/mnt/build-cache"),
                access: crate::targets::MountAccess::Rw,
            },
        ];

        assert!(
            enforce_overlay_capable_mounts(&podman_target(), &mut mounts, &UnusedExecutor)
                .is_empty()
        );
    }

    #[test]
    fn failed_new_session_provisioning_retains_error_record() {
        let session_id = "0123456789abcdef0123456789abcdef";
        let record = SessionRecord {
            build_cache: None,
            container_workspace: None,
            mjolnir_subagents: None,
            create_managed_worktree: None,
            workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: session_id.into(),
            title: "new session".into(),
            harness_kind: mj_core::config::HarnessKind::Codex,
            last_profile: "codex".into(),
            bundle_id: "project".into(),
            project_directory: None,
            managed_worktree: None,
            target_template_id: "podman".into(),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state: SessionState::Provisioning,
            target: None,
            native_session_id: None,
            acp_session_title: None,
            session_title_override: None,
            created_at: "2026-08-12T00:00:00Z".into(),
            updated_at: "2026-08-12T00:00:00Z".into(),
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        };
        let mut state = State::default();
        state.sessions.insert(session_id.into(), record);

        let result = apply_new_session_provisioning_result(
            &mut state,
            session_id,
            Err(anyhow::anyhow!("container creation failed")),
        );

        assert!(result.is_err());
        let retained = &state.sessions[session_id];
        assert_eq!(retained.state, SessionState::Error);
        assert!(retained.target.is_none());
        assert!(
            retained
                .last_error
                .as_deref()
                .unwrap()
                .contains("container creation failed")
        );
    }

    const SSH_DOCKER_FAILURE_CHILD: &str = "MJ_TEST_SSH_DOCKER_FAILURE_CHILD";

    #[test]
    fn failed_ssh_docker_preflight_retains_durable_error_record() {
        if std::env::var_os(SSH_DOCKER_FAILURE_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let test = "failed_ssh_docker_preflight_retains_durable_error_record";
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    &format!("controller::provisioning::tests::{test}"),
                    "--nocapture",
                ])
                .env(SSH_DOCKER_FAILURE_CHILD, "1")
                .env("MJ_DATA_DIR", directory.path())
                .env("MJ_CONFIG_DIR", directory.path());
            let output = mj_core::subprocess::run_with_input(&mut command, &[]).unwrap();
            assert!(
                output.status.success(),
                "isolated {test} failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let _writer = crate::database::install_isolated_test_writer();
        let config = ssh_docker_registration_config();
        config.save().unwrap();
        let mut controller = Controller {
            config,
            state: State::default(),
        };
        let session_id = controller
            .register_session_with_resources(
                "codex",
                "project",
                "docker",
                "failed image",
                SessionLaunchOptions {
                    mjolnir_subagents: None,
                    create_managed_worktree: None,
                    initial_prompt: None,
                    workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
                    additional_mounts: Vec::new(),
                    resource_allocation: None,
                    project_directory: None,
                    session_title_override: None,
                },
            )
            .unwrap();
        assert!(
            crate::database::load_state()
                .unwrap()
                .sessions
                .contains_key(&session_id)
        );

        let executor = RecordingExecutor::failing("check Docker daemon");
        let error =
            futures::executor::block_on(controller.provision_session_with_failure_disposition(
                &session_id,
                &executor,
                None,
                ProvisioningFailureDisposition::Discard,
            ))
            .unwrap_err();
        let reported = format!("{error:#}");
        assert!(
            reported.contains("remote Docker preflight failed"),
            "{reported}"
        );
        assert!(
            executor.commands().iter().any(|argv| {
                let command = argv.join(" ");
                command.contains("'docker' 'version'")
            }),
            "the fake preflight did not run: {:?}",
            executor.commands()
        );
        let retained = &controller.state.sessions[&session_id];
        assert_eq!(retained.state, SessionState::Error);
        assert!(retained.target.is_none());

        let reloaded = Controller::load().unwrap();
        let retained = &reloaded.state.sessions[&session_id];
        assert_eq!(retained.state, SessionState::Error);
        assert!(retained.target.is_none());
        assert!(retained.last_error.is_some());
    }

    /// A sub-agent create has no waiter, so a failure that happens before the
    /// first command has to land in the child's own record.
    #[test]
    fn subagent_placement_failure_marks_the_child_record_in_error() {
        if std::env::var_os(SSH_DOCKER_FAILURE_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let test = "subagent_placement_failure_marks_the_child_record_in_error";
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    &format!("controller::provisioning::tests::{test}"),
                    "--nocapture",
                ])
                .env(SSH_DOCKER_FAILURE_CHILD, "1")
                .env("MJ_DATA_DIR", directory.path())
                .env("MJ_CONFIG_DIR", directory.path());
            let output = mj_core::subprocess::run_with_input(&mut command, &[]).unwrap();
            assert!(
                output.status.success(),
                "isolated {test} failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let _writer = crate::database::install_isolated_test_writer();
        let config = ssh_docker_registration_config();
        config.save().unwrap();
        let mut controller = Controller {
            config,
            state: State::default(),
        };
        let child_id = controller
            .register_session_with_resources(
                "codex",
                "project",
                "docker",
                "borrow the parent container",
                SessionLaunchOptions {
                    mjolnir_subagents: None,
                    create_managed_worktree: None,
                    initial_prompt: None,
                    workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
                    additional_mounts: Vec::new(),
                    resource_allocation: None,
                    project_directory: None,
                    session_title_override: None,
                },
            )
            .unwrap();

        // The shape `register_subagent` produced before borrowing was
        // recorded: the parent's container with no owner, which no child can
        // verify.
        let record = controller.state.sessions.get_mut(&child_id).unwrap();
        record.target = Some(mj_core::state::TargetLocator::SshDocker {
            host: "builder".into(),
            container_id: targets::resource_name("0123456789abcdef0123456789abcdef").unwrap(),
            borrowed_from: None,
        });
        crate::database::save_lifecycle_session(record).unwrap();

        let executor = RecordingExecutor::succeeding();
        let error = futures::executor::block_on(
            controller.provision_subagent_session_controlled(&child_id, &executor),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("refusing cleanup: container locator"),
            "unexpected error: {error:#}"
        );
        assert!(
            executor.commands().is_empty(),
            "placement failed, so nothing should have run: {:?}",
            executor.commands()
        );

        for record in [
            controller.state.sessions[&child_id].clone(),
            Controller::load().unwrap().state.sessions[&child_id].clone(),
        ] {
            assert_eq!(record.state, SessionState::Error);
            assert!(
                record
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.starts_with("sub-agent startup failed:")),
                "unexpected durable error: {:?}",
                record.last_error
            );
        }
    }

    #[test]
    fn failed_node_preflight_retains_error_before_provisioning() {
        if std::env::var_os(SSH_DOCKER_FAILURE_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let test = "failed_node_preflight_retains_error_before_provisioning";
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    &format!("controller::provisioning::tests::{test}"),
                    "--nocapture",
                ])
                .env(SSH_DOCKER_FAILURE_CHILD, "1")
                .env("MJ_DATA_DIR", directory.path())
                .env("MJ_CONFIG_DIR", directory.path());
            let output = mj_core::subprocess::run_with_input(&mut command, &[]).unwrap();
            assert!(
                output.status.success(),
                "isolated {test} failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let _writer = crate::database::install_isolated_test_writer();
        let mut config = ssh_docker_registration_config();
        config.targets.insert(
            "docker".into(),
            TargetTemplate::SshBare {
                ssh: SshConnection {
                    host: "builder".into(),
                    user: Some("agent".into()),
                    identity_file: None,
                    extra_args: Vec::new(),
                },
                permissions: mj_core::config::PermissionMode::Guardian,
                workspace_prefix: PathBuf::from(".local/share/hel/workspaces"),
            },
        );
        config.save().unwrap();
        let mut controller = Controller {
            config,
            state: State::default(),
        };
        let session_id = controller
            .register_session_with_resources(
                "codex",
                "project",
                "docker",
                "missing Node",
                SessionLaunchOptions {
                    mjolnir_subagents: None,
                    create_managed_worktree: None,
                    initial_prompt: None,
                    workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
                    additional_mounts: Vec::new(),
                    resource_allocation: None,
                    project_directory: Some("/srv/project".into()),
                    session_title_override: None,
                },
            )
            .unwrap();
        assert!(
            crate::database::load_state()
                .unwrap()
                .sessions
                .contains_key(&session_id)
        );

        let executor = RecordingExecutor::failing("preflight managed harness Node.js and npm");
        let error =
            futures::executor::block_on(controller.provision_session_with_failure_disposition(
                &session_id,
                &executor,
                None,
                ProvisioningFailureDisposition::Discard,
            ))
            .unwrap_err();
        let reported = format!("{error:#}");
        assert!(reported.contains("Node.js 22+ and npm"), "{reported}");
        assert_eq!(
            executor.commands().len(),
            1,
            "preflight must fail before provisioning"
        );
        let retained = &controller.state.sessions[&session_id];
        assert_eq!(retained.state, SessionState::Error);
        assert!(retained.target.is_none());

        let reloaded = Controller::load().unwrap();
        let retained = &reloaded.state.sessions[&session_id];
        assert_eq!(retained.state, SessionState::Error);
        assert!(retained.target.is_none());
        assert!(retained.last_error.is_some());
    }

    #[test]
    fn failed_new_worker_start_retains_session_only_after_target_cleanup() {
        let session_id = "0123456789abcdef0123456789abcdef";
        let mut session = SessionRecord {
            build_cache: None,
            container_workspace: None,
            mjolnir_subagents: None,
            create_managed_worktree: None,
            workspace_id: mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: session_id.into(),
            title: "new session".into(),
            harness_kind: mj_core::config::HarnessKind::Kimi,
            last_profile: "kimi".into(),
            bundle_id: "raw-project".into(),
            project_directory: Some("/srv/project".into()),
            managed_worktree: None,
            target_template_id: "remote".into(),
            resource_allocation: None,
            additional_mounts: Vec::new(),
            state: SessionState::Disconnected,
            target: Some(TargetLocator::SshBare {
                host: "builder".into(),
                workspace: format!(".local/share/hel/workspaces/{session_id}").into(),
                worker_id: None,
            }),
            native_session_id: None,
            acp_session_title: None,
            session_title_override: None,
            created_at: "2026-08-12T00:00:00Z".into(),
            updated_at: "2026-08-12T00:00:00Z".into(),
            viewed_through_event_ordinal: 0,
            draft_input: String::new(),
            last_error: None,
            last_checkpoint_error: None,
            checkpoint: None,
        };
        let mut cleaned = State::default();
        cleaned.sessions.insert(session_id.into(), session.clone());

        let failure =
            apply_failed_new_session_rollback(&mut cleaned, session_id, "ACP startup failed", None);

        let retained = &cleaned.sessions[session_id];
        assert_eq!(retained.state, SessionState::Error);
        assert!(retained.target.is_none());
        assert!(failure.to_string().contains("failed session retained"));

        session.state = SessionState::Disconnected;
        let mut cleanup_failed = State::default();
        cleanup_failed.sessions.insert(session_id.into(), session);
        let failure = apply_failed_new_session_rollback(
            &mut cleanup_failed,
            session_id,
            "ACP startup failed",
            Some("ssh unavailable".into()),
        );
        let retained = cleanup_failed.sessions.get(session_id).unwrap();
        assert_eq!(retained.state, SessionState::Error);
        assert!(retained.target.is_some());
        assert!(failure.to_string().contains("cleanup"));
    }
    #[test]
    fn launch_failure_is_persisted_separately_from_session_state() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let detail = format!(
            "specific startup cause\n{}\nstderr tail survives",
            "x".repeat(MAX_LAUNCH_DIAGNOSTIC_BYTES)
        );

        let path = persist_launch_failure_to(directory.path(), session_id, &detail).unwrap();
        let saved = std::fs::read_to_string(path).unwrap();

        assert!(saved.contains("specific startup cause"));
        assert!(saved.contains("launch diagnostic truncated"));
        assert!(saved.contains("stderr tail survives"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(directory.path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn noting_a_launch_failure_writes_the_diagnostic_and_returns_the_reason() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let error =
            anyhow::anyhow!("connect worker").context("Connection closed by 10.0.0.1 port 22");

        let detail = note_new_session_launch_failure_in(directory.path(), session_id, &error);

        assert!(
            detail.contains("Connection closed by 10.0.0.1 port 22"),
            "the returned reason keeps the underlying error text"
        );
        assert!(
            detail.contains("full diagnostic saved to"),
            "the reason points at the saved diagnostic"
        );
        let saved = std::fs::read_to_string(
            directory
                .path()
                .join(format!("{session_id}-launch-error.txt")),
        )
        .unwrap();
        assert!(saved.contains("Connection closed by 10.0.0.1 port 22"));
    }
    #[test]
    fn inherited_git_settings_allow_only_portable_non_executable_values() {
        let settings = parse_inherited_git_settings(
                b"user.name\nAgent User\0USER.EMAIL\nagent@example.test\0pull.rebase\ntrue\0alias.deploy\n!ship\0credential.helper\nstore\0core.editor\nvim\0include.path\n/host/config\0user.name\nFinal User\0",
            )
            .unwrap();

        assert_eq!(
            settings,
            BTreeMap::from([
                ("pull.rebase".into(), "true".into()),
                ("user.email".into(), "agent@example.test".into()),
                ("user.name".into(), "Final User".into()),
            ])
        );
    }
    #[test]
    fn inherited_git_settings_reject_malformed_or_non_utf8_output() {
        assert!(parse_inherited_git_settings(b"user.name\0").is_err());
        assert!(parse_inherited_git_settings(b"user.name\n\xff\0").is_err());
    }
    #[test]
    fn inherited_git_settings_target_only_isolated_workers() {
        let ssh = SshTarget {
            destination: "worker@example.test".into(),
            ssh_args: vec!["-p".into(), "2222".into()],
        };
        let ephemeral = [
            targets::TargetLocator::LocalPodman {
                borrowed_from: None,
                container_id: "abcdef012345".into(),
                workspace_storage: Default::default(),
            },
            targets::TargetLocator::AppleContainer {
                borrowed_from: None,
                container_id: "abcdef012346".into(),
            },
            targets::TargetLocator::AwsEc2 {
                profile: "default".into(),
                region: "us-east-1".into(),
                instance_id: "i-1234567890abcdef0".into(),
                ssh: ssh.clone(),
                workspace: ".local/share/hel/workspaces/018f9dd2-a3b4-7c8d-9000-123456789abc"
                    .into(),
            },
            targets::TargetLocator::SshPodman {
                borrowed_from: None,
                ssh: ssh.clone(),
                container_id: "abcdef012347".into(),
                workspace_storage: Default::default(),
            },
        ];
        for locator in &ephemeral {
            assert!(inherits_controller_git_settings(locator));
            let commands = inherited_git_setting_commands(
                locator,
                "018f9dd2-a3b4-7c8d-9000-123456789abc",
                BTreeMap::from([("user.name".into(), "- Agent O'Brien 日本語".into())]),
            )
            .unwrap();
            assert_eq!(commands.len(), 1);
            assert!(
                commands[0]
                    .args
                    .iter()
                    .any(|argument| argument.contains("user.name"))
            );
            assert!(
                commands[0]
                    .args
                    .iter()
                    .any(|argument| argument.contains("- Agent O'"))
            );
        }

        let persistent = targets::TargetLocator::SshBare {
            worker_id: None,
            ssh,
            workspace: "/srv/hel/018f9dd2-a3b4-7c8d-9000-123456789abc".into(),
        };
        let local = targets::TargetLocator::LocalBare {
            worker_root: "/var/lib/hel/workers/018f9dd2-a3b4-7c8d-9000-123456789abc".into(),
        };
        assert!(!inherits_controller_git_settings(&persistent));
        assert!(!inherits_controller_git_settings(&local));
        assert!(
            inherited_git_setting_commands(
                &persistent,
                "018f9dd2-a3b4-7c8d-9000-123456789abc",
                BTreeMap::from([("user.name".into(), "Agent".into())]),
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn raw_ssh_targets_select_permissions_and_ssh_podman_is_unconstrained() {
        let ssh = mj_core::config::SshConnection {
            host: "builder".into(),
            user: None,
            identity_file: None,
            extra_args: Vec::new(),
        };
        let guardian = TargetTemplate::SshBare {
            ssh: ssh.clone(),
            permissions: mj_core::config::PermissionMode::Guardian,
            workspace_prefix: ".local/share/hel/workspaces".into(),
        };
        let podman = TargetTemplate::SshPodman {
            ssh: ssh.clone(),
            container: mj_core::config::ContainerTemplate {
                build_cache: None,
                image: "example.invalid/agent:latest".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: BTreeMap::new(),
                workspace_storage: Default::default(),
            },
        };
        let yolo = TargetTemplate::SshBare {
            ssh,
            permissions: mj_core::config::PermissionMode::Yolo,
            workspace_prefix: ".local/share/hel/workspaces".into(),
        };

        assert_eq!(
            TargetTemplate::LocalBare.execution_policy(),
            mj_core::config::ExecutionPolicy::ConfiguredApprovals
        );
        assert_eq!(
            guardian.execution_policy(),
            mj_core::config::ExecutionPolicy::ConfiguredApprovals
        );
        assert_eq!(
            podman.execution_policy(),
            mj_core::config::ExecutionPolicy::Unconstrained
        );
        assert_eq!(
            yolo.execution_policy(),
            mj_core::config::ExecutionPolicy::Unconstrained
        );
    }
    const PROVISIONED_SESSION: &str = "0123456789abcdef0123456789abcdef";

    /// Records every command a plan runs, and fails the one whose purpose it
    /// was told to fail.
    struct RecordingExecutor {
        failing_purpose: String,
        commands: Mutex<Vec<Vec<String>>>,
    }

    impl RecordingExecutor {
        fn failing(purpose: impl Into<String>) -> Self {
            Self {
                failing_purpose: purpose.into(),
                commands: Mutex::new(Vec::new()),
            }
        }

        fn succeeding() -> Self {
            Self::failing(String::new())
        }

        fn commands(&self) -> Vec<Vec<String>> {
            self.commands.lock().unwrap().clone()
        }
    }

    impl CommandExecutor for RecordingExecutor {
        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            let mut argv = vec![command.program.clone()];
            argv.extend(command.args.clone());
            self.commands.lock().unwrap().push(argv);
            Ok(CommandOutput {
                status: i32::from(command.purpose == self.failing_purpose),
                stdout: Vec::new(),
                stderr: b"the step failed".to_vec(),
            })
        }
    }

    fn container_targets() -> Vec<targets::TargetTemplate> {
        let container = ContainerTemplate {
            build_cache: None,
            image: "ubuntu:24.04".into(),
            pull_policy: Default::default(),
            extra_run_args: Vec::new(),
            workspace_storage: Default::default(),
        };
        vec![
            targets::TargetTemplate::LocalPodman(container.clone()),
            targets::TargetTemplate::AppleContainer(container.clone()),
            targets::TargetTemplate::SshPodman {
                ssh: SshTarget {
                    destination: "dev@example.test".into(),
                    ssh_args: vec!["-o".into(), "BatchMode=yes".into()],
                },
                container,
            },
        ]
    }

    #[test]
    fn a_failure_after_the_container_exists_removes_it_and_keeps_the_original_error() {
        let name = targets::resource_name(PROVISIONED_SESSION).unwrap();
        for target in container_targets() {
            let plan = targets::provision_plan(
                &target,
                PROVISIONED_SESSION,
                &probe_bundle(),
                &[],
                None,
                None,
            )
            .unwrap();
            let executor = RecordingExecutor::failing("clone app");

            let error = provision_target(&plan, &target, PROVISIONED_SESSION, &executor, |_| {
                unreachable!("locator discovery must not run after a failed plan")
            })
            .unwrap_err();

            let reported = format!("{error:#}");
            assert!(reported.contains("clone app failed"), "{reported}");
            assert!(reported.contains("cleanup succeeded"), "{reported}");
            // Remote commands reach the target posix-quoted.
            let removal = executor
                .commands()
                .into_iter()
                .map(|arguments| arguments.join(" ").replace('\'', ""))
                .find(|command| command.contains("rm --force") && command.contains(&name))
                .expect("cleanup removes the exact provisioned container");
            assert!(removal.contains("rm --force"), "{removal}");
            assert!(removal.contains(&name), "{removal}");
        }
    }

    #[test]
    fn target_creation_returns_repository_setup_without_running_it() {
        let target = podman_target();
        let plan = targets::provision_plan(
            &target,
            PROVISIONED_SESSION,
            &probe_bundle(),
            &[],
            None,
            None,
        )
        .unwrap();
        let executor = RecordingExecutor::succeeding();

        let (_, repositories) =
            provision_target_creation(&plan, &target, PROVISIONED_SESSION, &executor, |_| {
                Ok(TargetLocator::LocalPodman {
                    borrowed_from: None,
                    container_id: targets::resource_name(PROVISIONED_SESSION)?,
                    workspace_storage: Default::default(),
                })
            })
            .unwrap();

        assert_eq!(executor.commands().len(), 1, "only podman run may execute");
        assert!(
            repositories
                .commands
                .iter()
                .any(|command| command.purpose == "clone app")
        );
    }

    #[test]
    fn a_target_whose_creation_failed_is_never_torn_down() {
        for target in container_targets() {
            let plan = targets::provision_plan(
                &target,
                PROVISIONED_SESSION,
                &probe_bundle(),
                &[],
                None,
                None,
            )
            .unwrap();
            let creation = plan.split_at_target_creation().unwrap().0;
            let executor =
                RecordingExecutor::failing(creation.commands.last().unwrap().purpose.clone());

            let error = provision_target(&plan, &target, PROVISIONED_SESSION, &executor, |_| {
                unreachable!("locator discovery must not run after a failed plan")
            })
            .unwrap_err();

            let reported = format!("{error:#}");
            assert!(!reported.contains("cleanup"), "{reported}");
            assert!(
                !executor
                    .commands()
                    .iter()
                    .any(|argv| argv.join(" ").contains("rm --force")),
                "{:?}",
                executor.commands()
            );
        }
    }

    #[test]
    fn a_target_whose_locator_cannot_be_discovered_is_removed_again() {
        let target = podman_target();
        let plan = targets::provision_plan(
            &target,
            PROVISIONED_SESSION,
            &probe_bundle(),
            &[],
            None,
            None,
        )
        .unwrap();
        let executor = RecordingExecutor::succeeding();

        let error = provision_target(&plan, &target, PROVISIONED_SESSION, &executor, |_| {
            bail!("the container never reported an address")
        })
        .unwrap_err();

        let reported = format!("{error:#}");
        assert!(reported.contains("never reported an address"), "{reported}");
        assert!(reported.contains("cleanup succeeded"), "{reported}");
        let removal = executor
            .commands()
            .into_iter()
            .map(|arguments| arguments.join(" "))
            .find(|command| command.contains("podman rm --force --ignore"))
            .expect("cleanup removes the provisioned Podman container");
        assert!(removal.contains("podman rm --force --ignore"), "{removal}");
    }

    /// A raw project directory is the user's own: provisioning it creates
    /// nothing that a failure could leak.
    #[test]
    fn a_bare_project_failure_removes_nothing() {
        let target = targets::TargetTemplate::LocalBare;
        let plan =
            targets::provision_bare_project_plan(&target, PROVISIONED_SESSION, "/srv/project")
                .unwrap();
        let executor = RecordingExecutor::succeeding();

        let error = provision_target(&plan, &target, PROVISIONED_SESSION, &executor, |_| {
            bail!("the worker root was unreadable")
        })
        .unwrap_err();

        assert!(!format!("{error:#}").contains("cleanup"));
        assert!(executor.commands().is_empty());
    }
}

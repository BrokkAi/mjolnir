//! Session provisioning, rollback, and worker-side Git bootstrap.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};

use crate::hel_git_proxy::{GitBrokerSpec, broker_is_alive, running_broker_pid};
use hel::hel_archive::{
    ArchiveInput, BundleManifest, GitCollectionSpec, GitHistoryMode, SessionManifest, SystemGit,
    TargetManifest, collect_git_snapshot, write_archive_atomic,
};
use hel::hel_checkpoint::RepositoryRestoreSpec;
use hel::hel_config::{ProjectBundle, TargetTemplate, atomic_write, data_dir};
use hel::hel_local_git::canonical_repository;
use hel::hel_projection::canonical_session_from_materialized;
use hel::hel_state::{HelState, SessionRecord, SessionState, TargetLocator};
use hel::hel_targets::{
    self, CancellableProcessExecutor, CommandExecutor, CommandOutput, CommandSpec, ProvisionStage,
    ProvisionStageGuard,
};

use super::backend::{
    ContainerOverrides, absolute_target_path, backend_bundle, backend_locator, backend_target,
    configure_github_token_environment, controller_github_token, locator_after_provision,
    preflight_target, use_github_https_urls,
};
use super::checkpoint::upload_checkpoint_spec;
use super::git_cache;
use super::readiness::{connect_started_worker, wait_for_native_session};
use super::worker_binary::{start_worker, worker_probe_diagnosis};
use super::{Controller, execute_checked, now, target_kind};

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

/// Whether connecting local repositories may also carry the user's current
/// uncommitted changes into a still-empty target checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LocalBootstrap {
    /// A fresh target starts from `git init`, so seed its branch and dirty
    /// state from the local repository.
    Seed,
    /// Seed from this checkout instead of the bundle's configured path. A
    /// resume that moves a raw session into a target carries the session's own
    /// worktree, not the user's primary checkout.
    SeedFrom(PathBuf),
    /// Resume restores the session's own dirty state from the checkpoint
    /// archive; seeding the local repository's would collide with it.
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProvisioningFailureDisposition {
    /// A freshly registered session has no durable history to retain.
    Discard,
    /// Resume owns rollback to the archived record and checkpoint lineage.
    Preserve,
}

impl Controller {
    pub async fn provision_session_controlled(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        self.provision_session_controlled_with_commit(session_id, executor, || Ok(()))
            .await
    }

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
                self.connect_and_start_worker(session_id, executor, &backend, &worker_root)
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
        // The broker bridges into the target, so it is stopped before the
        // target goes away. A launch this rollback discards is over: nothing
        // will connect to its local origin again.
        let broker_cleanup = retire_git_broker(session_id);
        let target_cleanup = match session.target.as_ref() {
            Some(locator) => (|| -> Result<()> {
                let backend = backend_locator(locator, &session, &self.config)?;
                hel_targets::close_plan(&backend, session_id)?
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
        let cleanup_error = [broker_cleanup, target_cleanup, worktree_cleanup]
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
        let original = format!("{error:#}");
        let original = match persist_launch_failure(session_id, &original) {
            Ok(path) => format!("{original}; full diagnostic saved to {}", path.display()),
            Err(save_error) => {
                format!("{original}; saving the local diagnostic failed: {save_error:#}")
            }
        };
        let failure = apply_failed_new_session_rollback(
            &mut self.state,
            session_id,
            &original,
            (!cleanup_error.is_empty()).then_some(cleanup_error),
        );
        self.persist_session_state(session_id)?;
        Ok(failure)
    }

    pub async fn provision_session_with(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        self.provision_session_with_github_token(session_id, executor, None)
            .await
    }

    async fn provision_session_with_github_token(
        &mut self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
        github_token: Option<&str>,
    ) -> Result<()> {
        self.provision_session_with_failure_disposition(
            session_id,
            executor,
            github_token,
            ProvisioningFailureDisposition::Discard,
        )
        .await
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
    ) -> Result<hel_targets::CommandPlan> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        if session.state != SessionState::Provisioning {
            bail!("session {session_id} is not provisioning");
        }
        let created_worktree = match self.prepare_managed_raw_worktree(session_id, executor) {
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
            let mut runtime_mounts = if matches!(target, hel_targets::TargetTemplate::AwsEc2(_)) {
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
            let mut bundle = session
                .project_directory
                .is_none()
                .then(|| self.config.bundles.get(&session.bundle_id))
                .flatten()
                .map(backend_bundle)
                .transpose()?;
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
            let provision = if let Some(project_directory) = &session.project_directory {
                hel_targets::provision_bare_project_plan(
                    &target,
                    session_id,
                    &project_directory.to_string_lossy(),
                )
            } else {
                bundle
                    .as_ref()
                    .context("project bundle disappeared during provisioning")
                    .and_then(|bundle| {
                        hel_targets::provision_plan(&target, session_id, bundle, &runtime_mounts)
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
                .map(|(locator, remainder)| (locator, remainder, bundle));
            if result.is_err()
                && let Some(cache) = &prepared_cache
            {
                if let Some(locator) = provisioned_locator(&target, session_id, None) {
                    let _ = hel_targets::close_plan(&locator, session_id)
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
                apply_new_session_provisioning_result(&mut self.state, session_id, Err(error))?;
                unreachable!("an unsuccessful provisioning result returned Ok")
            }
            Ok((locator, remainder, bundle)) => {
                apply_new_session_provisioning_result(&mut self.state, session_id, Ok(locator))?;
                let session = &self.state.sessions[session_id];
                let backend = backend_locator(
                    session
                        .target
                        .as_ref()
                        .context("provisioned target disappeared")?,
                    session,
                    &self.config,
                )?;
                if matches!(backend, hel_targets::TargetLocator::AwsEc2 { .. }) {
                    hel_targets::provision_on_locator_plan(
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
                hel::hel_database::remember_project_directory(host, &directory)?;
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
        hel::hel_database::mark_session_worker_connected(
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
    ) -> Result<(hel_targets::TargetLocator, String)> {
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
        backend: &hel_targets::TargetLocator,
        worker_root: &str,
    ) -> Result<Option<String>> {
        // A local-origin fetch needs both the checkout from the clone lane and
        // the worker binary from the sync lane, so it joins them here.
        {
            let syncing = &StagedExecutor::new(executor, ProvisionStage::Syncing);
            install_inherited_git_settings(executor, backend, session_id)?;
            self.connect_local_repositories(
                session_id,
                backend,
                worker_root,
                syncing,
                LocalBootstrap::Seed,
            )?;
        }
        let executor = &StagedExecutor::new(executor, ProvisionStage::Starting);
        start_worker(executor, backend, worker_root)?;
        let reconnect = &hel_targets::reconnect_plan(backend, session_id)?.commands[0];
        let readiness = async {
            let mut relay =
                connect_started_worker(reconnect, session_id, executor, backend, worker_root)
                    .await?;
            let native_session_id = wait_for_native_session(&mut relay, executor).await?;
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

    /// Point the target's checkouts at the `hel-local` Git proxy and fetch the
    /// committed history it serves. `bootstrap` decides whether a still-empty
    /// checkout is also seeded with the local repository's uncommitted changes.
    pub(super) fn connect_local_repositories(
        &self,
        session_id: &str,
        backend: &hel_targets::TargetLocator,
        worker_root: &str,
        executor: &impl CommandExecutor,
        bootstrap: LocalBootstrap,
    ) -> Result<()> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        if session.project_directory.is_some() {
            return Ok(());
        }
        let bundle = self
            .config
            .bundles
            .get(&session.bundle_id)
            .context("session bundle is missing")?;
        let local = bundle
            .repositories
            .iter()
            .filter_map(|repository| repository.local.as_ref().map(|path| (repository, path)))
            .collect::<Vec<_>>();
        if local.is_empty() {
            return Ok(());
        }

        let absolute_worker_root =
            absolute_target_path(executor, backend, session_id, worker_root)?;
        let repositories = local
            .iter()
            .map(|(repository, path)| Ok((repository.id.clone(), canonical_repository(path)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        ensure_git_broker(session_id, backend, repositories)?;

        let workspace_root = match backend {
            hel_targets::TargetLocator::LocalPodman { .. }
            | hel_targets::TargetLocator::LocalDocker { .. }
            | hel_targets::TargetLocator::AppleContainer { .. }
            | hel_targets::TargetLocator::SshPodman { .. } => "/workspace".to_owned(),
            hel_targets::TargetLocator::AwsEc2 { workspace, .. }
            | hel_targets::TargetLocator::SshBare { workspace, .. } => workspace.clone(),
            hel_targets::TargetLocator::LocalBare { worker_root } => worker_root.clone(),
        };
        let mut missing = Vec::new();
        for &(repository, source) in &local {
            local_branch(source)?;
            let destination = format!(
                "{workspace_root}/{}",
                repository.destination.to_string_lossy()
            );
            let origin = local_origin_url(&absolute_worker_root, &repository.id);
            for (args, purpose) in [
                (
                    vec![
                        "git".into(),
                        "-C".into(),
                        destination.clone(),
                        "config".into(),
                        "protocol.ext.allow".into(),
                        "always".into(),
                    ],
                    "enable the confined local Git transport",
                ),
                (
                    vec![
                        "git".into(),
                        "-C".into(),
                        destination.clone(),
                        "config".into(),
                        "remote.origin.url".into(),
                        origin,
                    ],
                    "configure local Git origin",
                ),
                (
                    vec![
                        "git".into(),
                        "-C".into(),
                        destination.clone(),
                        "config".into(),
                        "remote.origin.fetch".into(),
                        "+refs/heads/*:refs/remotes/origin/*".into(),
                    ],
                    "configure local Git fetch refspec",
                ),
            ] {
                execute_checked(
                    executor,
                    hel_targets::command_on_locator(backend, session_id, args, purpose)?,
                )?;
            }
            let has_head = executor.execute(&hel_targets::command_on_locator(
                backend,
                session_id,
                vec![
                    "git".into(),
                    "-C".into(),
                    destination.clone(),
                    "rev-parse".into(),
                    "--verify".into(),
                    "HEAD".into(),
                ],
                "inspect local Git bootstrap state",
            )?)?;
            if has_head.status != 0 {
                missing.push((repository, source));
            }
        }
        // Fetch before bootstrapping: the proxy delivers every branch, so the
        // bootstrap archive only has to carry identity and dirty state, and
        // the commit it checks out is already present.
        for (repository, _) in &local {
            let destination = format!(
                "{workspace_root}/{}",
                repository.destination.to_string_lossy()
            );
            execute_checked(
                executor,
                hel_targets::command_on_locator(
                    backend,
                    session_id,
                    vec![
                        "git".into(),
                        "-C".into(),
                        destination.clone(),
                        "fetch".into(),
                        "origin".into(),
                    ],
                    "fetch local Git origin",
                )?,
            )?;
        }
        if let Some(sources) = seed_sources(&missing, &bootstrap)
            && !sources.is_empty()
        {
            bootstrap_local_repositories(
                executor,
                backend,
                session,
                bundle,
                &workspace_root,
                worker_root,
                &sources,
            )?;
        }
        Ok(())
    }
}

const MAX_LAUNCH_DIAGNOSTIC_BYTES: usize = 64 * 1024;

const RETAINED_LAUNCH_DIAGNOSTICS: usize = 20;

fn persist_launch_failure(session_id: &str, detail: &str) -> Result<PathBuf> {
    persist_launch_failure_to(&data_dir().join("diagnostics"), session_id, detail)
}

fn persist_launch_failure_to(directory: &Path, session_id: &str, detail: &str) -> Result<PathBuf> {
    hel::hel_config::validate_id("session", session_id)?;
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
    state: &mut HelState,
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
            state.sessions.remove(session_id);
            Err(error)
        }
    }
}

pub(super) fn apply_failed_new_session_rollback(
    state: &mut HelState,
    session_id: &str,
    original_error: &str,
    cleanup_error: Option<String>,
) -> anyhow::Error {
    match cleanup_error {
        None => {
            state.sessions.remove(session_id);
            anyhow::anyhow!(
                "{original_error}; partial target removed and provisional session discarded"
            )
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
    state: &HelState,
    session_id: &str,
    backend: &hel_targets::TargetLocator,
    worker_root: &str,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let hel_targets::TargetLocator::AwsEc2 { .. } = backend else {
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
        let install = hel_targets::command_on_locator(
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
        hel::hel_resources::stream_resource(&resource.source, |stream| {
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
                hel_targets::command_thread_panic_message(panic.as_ref())
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
    plan: &hel_targets::CommandPlan,
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
    plan: &hel_targets::CommandPlan,
    target: &hel_targets::TargetTemplate,
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
    plan: &hel_targets::CommandPlan,
    target: &hel_targets::TargetTemplate,
    session_id: &str,
    executor: &(impl CommandExecutor + Sync),
    discover: impl FnOnce(&[CommandOutput]) -> Result<TargetLocator>,
) -> Result<(TargetLocator, hel_targets::CommandPlan)> {
    let Some((creation, remainder)) = plan.split_at_target_creation() else {
        // Nothing this plan runs can leave a target behind, so its commands
        // must still finish before the locator is usable.
        let outputs = plan.execute_concurrent(executor)?;
        return discover(&outputs).map(|locator| {
            (
                locator,
                hel_targets::CommandPlan {
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
    target: &hel_targets::TargetTemplate,
    session_id: &str,
    create_output: Option<&CommandOutput>,
    executor: &impl CommandExecutor,
) -> Option<String> {
    let locator = provisioned_locator(target, session_id, create_output)?;
    let leak = format!(
        "the resource may still exist; find it via its dev.mj.session={session_id} label/tag"
    );
    let plan = match hel_targets::close_plan(&locator, session_id) {
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
    match hel_targets::cleanup_target_is_confirmed_absent(&locator, session_id, executor) {
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
    target: &hel_targets::TargetTemplate,
    session_id: &str,
    create_output: Option<&CommandOutput>,
) -> Option<hel_targets::TargetLocator> {
    let container_id = || hel_targets::resource_name(session_id).ok();
    Some(match target {
        // A bare project directory belongs to the user: provisioning creates
        // nothing that a failure could leak.
        hel_targets::TargetTemplate::LocalBare => return None,
        hel_targets::TargetTemplate::LocalPodman(container) => {
            hel_targets::TargetLocator::LocalPodman {
                container_id: container_id()?,
                workspace_storage: hel_targets::podman_workspace_locator(container, session_id)
                    .ok()?,
            }
        }
        hel_targets::TargetTemplate::LocalDocker(_) => hel_targets::TargetLocator::LocalDocker {
            container_id: container_id()?,
        },
        hel_targets::TargetTemplate::AppleContainer(_) => {
            hel_targets::TargetLocator::AppleContainer {
                container_id: container_id()?,
            }
        }
        hel_targets::TargetTemplate::SshPodman { ssh, container } => {
            hel_targets::TargetLocator::SshPodman {
                ssh: ssh.clone(),
                container_id: container_id()?,
                workspace_storage: hel_targets::podman_workspace_locator(container, session_id)
                    .ok()?,
            }
        }
        hel_targets::TargetTemplate::SshBare { ssh, .. } => hel_targets::TargetLocator::SshBare {
            ssh: ssh.clone(),
            workspace: hel_targets::workspace_for(target, session_id).ok()?,
        },
        hel_targets::TargetTemplate::AwsEc2(aws) => hel_targets::TargetLocator::AwsEc2 {
            profile: aws.profile.clone(),
            region: aws.region.clone(),
            instance_id: serde_json::from_slice::<serde_json::Value>(&create_output?.stdout)
                .ok()?
                .pointer("/Instances/0/InstanceId")?
                .as_str()?
                .to_owned(),
            ssh: aws.ssh.clone(),
            workspace: hel_targets::workspace_for(target, session_id).ok()?,
        },
    })
}

fn local_branch(repository: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .current_dir(repository)
        .output()
        .with_context(|| format!("read current branch in {}", repository.display()))?;
    if !output.status.success() {
        bail!(
            "local repository {} must have a branch checked out before Hel can expose it as origin",
            repository.display()
        );
    }
    let branch = String::from_utf8(output.stdout).context("decode local Git branch")?;
    let branch = branch.trim().to_owned();
    if branch.is_empty() {
        bail!("local repository has an empty current branch");
    }
    Ok(branch)
}

fn local_origin_url(worker_root: &str, repository_id: &str) -> String {
    fn ext_argument(value: &str) -> String {
        value.replace('%', "%%").replace(' ', "% ")
    }
    format!(
        "ext::{}/hel worker git-proxy --root {} --repository {} %S",
        ext_argument(worker_root),
        ext_argument(worker_root),
        repository_id,
    )
}

fn seed_sources<'a>(
    missing: &[(&'a hel::hel_config::ProjectRepository, &'a PathBuf)],
    bootstrap: &'a LocalBootstrap,
) -> Option<Vec<(&'a hel::hel_config::ProjectRepository, &'a PathBuf)>> {
    let checkout = match bootstrap {
        LocalBootstrap::Skip => return None,
        LocalBootstrap::Seed => None,
        LocalBootstrap::SeedFrom(checkout) => Some(checkout),
    };
    Some(
        missing
            .iter()
            .map(|(repository, source)| (*repository, checkout.unwrap_or(source)))
            .collect(),
    )
}

/// Carry a local repository's identity and uncommitted changes into a freshly
/// initialized target checkout. Committed history is never bundled here: the
/// caller fetches it through the `hel-local` proxy first.
fn bootstrap_local_repositories(
    executor: &impl CommandExecutor,
    locator: &hel_targets::TargetLocator,
    session: &SessionRecord,
    bundle: &ProjectBundle,
    workspace_root: &str,
    worker_root: &str,
    repositories: &[(&hel::hel_config::ProjectRepository, &PathBuf)],
) -> Result<()> {
    let snapshots = repositories
        .iter()
        .map(|(repository, source)| {
            collect_git_snapshot(
                &SystemGit,
                source,
                &GitCollectionSpec {
                    id: repository.id.clone(),
                    relative_destination: repository.destination.clone(),
                    history: GitHistoryMode::NoBundle,
                    origin_override: Some(format!("mj-local:{}", repository.id)),
                },
            )
            .with_context(|| format!("snapshot local repository {:?}", repository.id))
        })
        .collect::<Result<Vec<_>>>()?;
    let staging = data_dir().join("git-seeds");
    std::fs::create_dir_all(&staging)?;
    let archive_path = staging.join(format!("{}.hel.zip", session.id));
    write_archive_atomic(
        &archive_path,
        &ArchiveInput {
            session: SessionManifest {
                id: session.id.clone(),
                title: session.title.clone(),
                harness_kind: session.harness_kind,
                profile_id: session.last_profile.clone(),
                native_session_id: session.native_session_id.clone().unwrap_or_default(),
                created_at: session.created_at.clone(),
                checkpointed_at: now(),
                hel_version: env!("CARGO_PKG_VERSION").into(),
                relay_version: env!("CARGO_PKG_VERSION").into(),
                adapter_version: "acp-v1".into(),
            },
            target: TargetManifest {
                template_id: session.target_template_id.clone(),
                target_kind: target_kind(locator).into(),
                details: Default::default(),
            },
            bundle: BundleManifest {
                id: session.bundle_id.clone(),
                primary_repository: bundle.primary_repo.clone(),
            },
            canonical_session: canonical_session_from_materialized(
                &hel::hel_state::MaterializedSession::empty(session.id.clone()),
            )?,
            native_artifacts: Vec::new(),
            repositories: snapshots,
        },
    )?;

    let remote_archive = format!("{worker_root}/local-seed.hel.zip");
    let remote_spec = format!("{worker_root}/local-seed.json");
    let target_path = |path: &str| match locator {
        hel_targets::TargetLocator::AwsEc2 { .. } | hel_targets::TargetLocator::SshBare { .. } => {
            PathBuf::from(format!("~/{path}"))
        }
        _ => PathBuf::from(path),
    };
    let spec = RepositoryRestoreSpec {
        archive_path: target_path(&remote_archive),
        workspace_root: target_path(workspace_root),
    };
    let local_spec = staging.join(format!("{}.json", session.id));
    hel::hel_config::atomic_write(&local_spec, &serde_json::to_vec_pretty(&spec)?)?;
    upload_checkpoint_spec(
        executor,
        locator,
        &session.id,
        &archive_path,
        &remote_archive,
    )?;
    upload_checkpoint_spec(executor, locator, &session.id, &local_spec, &remote_spec)?;
    execute_checked(
        executor,
        hel_targets::command_on_locator(
            locator,
            &session.id,
            vec![
                format!("{worker_root}/hel"),
                "worker".into(),
                "restore-repositories".into(),
                "--spec".into(),
                remote_spec,
            ],
            "restore local repository bootstrap",
        )?,
    )?;
    Ok(())
}

/// The files one session's local Git broker is identified by.
#[derive(Debug, Clone)]
struct BrokerFiles {
    spec: PathBuf,
    ready: PathBuf,
    pid: PathBuf,
    log: PathBuf,
}

impl BrokerFiles {
    fn in_directory(directory: &Path, session_id: &str) -> Self {
        Self {
            spec: directory.join(format!("{session_id}.json")),
            ready: directory.join(format!("{session_id}.ready")),
            pid: directory.join(format!("{session_id}.pid")),
            log: directory.join(format!("{session_id}.log")),
        }
    }
}

/// How long a starting broker has to publish its ready marker.
const BROKER_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Consecutive restarts a supervisor attempts before it reports the session's
/// local origin as unserved.
const BROKER_RESTART_ATTEMPTS: u32 = 5;

/// Delay before the first restart; later attempts wait a multiple of it.
const BROKER_RESTART_BACKOFF: Duration = Duration::from_millis(250);

/// A run at least this long counts as healthy, so its ending starts a fresh
/// restart budget instead of continuing a restart storm.
const BROKER_HEALTHY_RUN: Duration = Duration::from_secs(30);

/// How long a retired broker has to exit after being asked, and again after
/// being killed, before the stop is reported as failed.
const BROKER_STOP_GRACE: Duration = Duration::from_secs(2);

/// How often a stopping broker's lock is re-tested.
const BROKER_STOP_POLL: Duration = Duration::from_millis(25);

fn broker_directory() -> PathBuf {
    data_dir().join("git-brokers")
}

fn ensure_git_broker(
    session_id: &str,
    locator: &hel_targets::TargetLocator,
    repositories: BTreeMap<String, PathBuf>,
) -> Result<()> {
    let directory = broker_directory();
    std::fs::create_dir_all(&directory)?;
    let files = BrokerFiles::in_directory(&directory, session_id);
    let spec = GitBrokerSpec {
        session_id: session_id.to_owned(),
        bridge: hel_targets::git_bridge_command(locator, session_id)?,
        repositories,
        ready_path: files.ready.clone(),
        pid_path: files.pid.clone(),
    };
    if broker_is_alive(&files.pid) {
        if broker_serves(&files, &spec) {
            return Ok(());
        }
        bail!(
            "a different local Git broker is still active for session {session_id}; close its target before reconnecting"
        );
    }
    spec.write(&files.spec)?;
    let child = match start_git_broker(&files) {
        Ok(child) => child,
        // A supervisor may have restarted this session's broker from the same
        // spec while this one was starting. That broker serves the session,
        // and only one of them can hold the session's broker lock.
        Err(error) if broker_serves(&files, &spec) => {
            tracing::debug!(
                session_id,
                error = format!("{error:#}"),
                "reused a concurrently started local Git broker"
            );
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    // A broker that dies later takes the session's `origin` remote with it,
    // so it is supervised rather than merely reaped.
    let session_id = session_id.to_owned();
    std::thread::spawn(move || supervise_git_broker(&session_id, &files, child));
    Ok(())
}

/// Whether a broker is already serving exactly this session and spec.
fn broker_serves(files: &BrokerFiles, spec: &GitBrokerSpec) -> bool {
    broker_is_alive(&files.pid)
        && files.ready.exists()
        && GitBrokerSpec::read(&files.spec).is_ok_and(|existing| &existing == spec)
}

/// Start the broker process and wait for it to publish its ready marker.
fn start_git_broker(files: &BrokerFiles) -> Result<std::process::Child> {
    // A broker killed outright leaves its marker behind, and the new one has
    // to publish its own before it counts as ready. A marker a live broker
    // owns is never touched.
    if !broker_is_alive(&files.pid)
        && let Err(error) = std::fs::remove_file(&files.ready)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %files.ready.display(),
            %error,
            "could not remove stale Git broker ready marker"
        );
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&files.log)
        .with_context(|| format!("open Git broker log {}", files.log.display()))?;
    let stderr = log.try_clone()?;
    let executable = std::env::current_exe().context("locate Hel controller executable")?;
    let mut command = Command::new(executable);
    command
        .args(["broker", "--spec"])
        .arg(&files.spec)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(stderr);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().context("start local Git broker")?;
    let deadline = Instant::now() + BROKER_READY_TIMEOUT;
    loop {
        if files.ready.exists() && broker_is_alive(&files.pid) {
            return Ok(child);
        }
        if let Some(status) = child.try_wait().context("poll local Git broker")? {
            bail!(
                "local Git broker exited with {status}; see {}",
                files.log.display()
            );
        }
        if Instant::now() >= deadline {
            // Leave no half-started broker behind holding the session slot.
            if let Err(error) = child.kill() {
                tracing::warn!(%error, "could not terminate timed-out Git broker");
            }
            if let Err(error) = child.wait() {
                tracing::warn!(%error, "could not reap timed-out Git broker");
            }
            bail!(
                "timed out starting local Git broker; see {}",
                files.log.display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Stop this session's local Git broker for good and clear the state that
/// would invite any controller to start another one.
///
/// Closing, deleting, or abandoning a session all end its local origin: the
/// target the broker bridges into is about to disappear, so a broker left
/// running would be restarted against nothing and finally reported as a
/// failure the user never caused.
pub(super) fn retire_git_broker(session_id: &str) -> Result<()> {
    retire_broker_files(&BrokerFiles::in_directory(&broker_directory(), session_id))
}

/// Retire one broker: signal the intent, stop the process, then remove the
/// files it left behind.
///
/// The spec is removed *first*, and that ordering is the whole design. Every
/// supervisor consults the spec before restarting, so its absence is how a
/// deliberate stop is told apart from a broker that died. Removing it after
/// the kill would race the supervisor into restarting a broker for a session
/// that is being torn down. The process is then stopped before its remaining
/// files go, so nothing is ever deleted under a live writer.
fn retire_broker_files(files: &BrokerFiles) -> Result<()> {
    remove_broker_file(&files.spec)?;
    stop_running_broker(&files.pid)?;
    remove_broker_file(&files.ready)?;
    remove_broker_file(&files.pid)?;
    // The log stays: it is where this session's Git failures were reported,
    // and reading it after the session ends is the point of keeping it.
    Ok(())
}

/// Whether this session still wants a broker that has stopped to be started
/// again.
///
/// A restart always spawns from the spec on disk, so a rewritten one needs no
/// special handling; a retired session's spec is gone, and a broker another
/// controller already has running belongs to that controller.
fn broker_needs_restart(files: &BrokerFiles) -> bool {
    files.spec.exists() && !broker_is_alive(&files.pid)
}

fn remove_broker_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove Git broker file {}", path.display()))
        }
    }
}

/// Terminate whatever broker still holds this session's slot, and wait for it
/// to let go of the lock.
///
/// The PID is re-read on every pass: a restart that was already in flight
/// when the session was retired claims the slot a moment later, and it has to
/// be stopped too.
fn stop_running_broker(pid_path: &Path) -> Result<()> {
    for escalate in [false, true] {
        let deadline = Instant::now() + BROKER_STOP_GRACE;
        loop {
            if !broker_is_alive(pid_path) {
                return Ok(());
            }
            if let Some(pid) = running_broker_pid(pid_path) {
                stop_broker_process_group(pid, escalate);
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(BROKER_STOP_POLL);
        }
    }
    bail!(
        "the local Git broker holding {} did not stop",
        pid_path.display()
    )
}

/// Signal the process group a broker leads. Brokers are started as their own
/// group leader, so this stops the target-side bridge with the broker instead
/// of leaving it attached to a target that is going away.
#[cfg(unix)]
fn stop_broker_process_group(pid: i32, escalate: bool) {
    hel::hel_subprocess::terminate_process_group(
        pid,
        if escalate {
            libc::SIGKILL
        } else {
            libc::SIGTERM
        },
    );
}

#[cfg(not(unix))]
fn stop_broker_process_group(_pid: i32, _escalate: bool) {}

/// Keep this session's Git broker running until another controller takes it
/// over, until the session retires it, or until restarting it stops helping.
fn supervise_git_broker(session_id: &str, files: &BrokerFiles, child: std::process::Child) {
    let mut started = Some(child);
    let outcome = supervise_broker_restarts(
        BROKER_RESTART_ATTEMPTS,
        || broker_needs_restart(files),
        || {
            let running = Instant::now();
            let mut child = match started.take() {
                Some(child) => child,
                None => start_git_broker(files)?,
            };
            let status = child.wait().context("wait for the local Git broker")?;
            Ok((running.elapsed(), format!("{status}")))
        },
        std::thread::sleep,
    );
    let Err(error) = outcome else {
        return;
    };
    tracing::error!(
        session_id,
        error = format!("{error:#}"),
        "the session's local Git origin is no longer served"
    );
    // Every broker error the user sees names this log, so the last word on
    // the broker belongs in it too.
    use std::io::Write as _;
    if let Ok(mut log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&files.log)
    {
        let _ = writeln!(log, "[hel {}] local Git origin lost: {error:#}", now());
    }
}

/// Restart the broker whenever it stops, until stopping is no longer this
/// supervisor's business or a run of restarts has failed to fix anything.
fn supervise_broker_restarts(
    attempts: u32,
    mut needs_restart: impl FnMut() -> bool,
    mut run: impl FnMut() -> Result<(Duration, String)>,
    mut back_off: impl FnMut(Duration),
) -> Result<()> {
    let mut consecutive = 0;
    loop {
        let failure = match run() {
            Ok((ran_for, status)) => {
                if ran_for >= BROKER_HEALTHY_RUN {
                    consecutive = 0;
                }
                anyhow::anyhow!("the local Git broker exited with {status}")
            }
            Err(error) => error,
        };
        if !needs_restart() {
            return Ok(());
        }
        consecutive += 1;
        if consecutive > attempts {
            return Err(failure.context(format!(
                "the local Git broker stopped {consecutive} times in a row and was not restarted again"
            )));
        }
        back_off(BROKER_RESTART_BACKOFF * consecutive);
    }
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
    target: &hel_targets::TargetTemplate,
    mounts: &mut [hel_targets::AdditionalMount],
    executor: &impl CommandExecutor,
) -> Vec<String> {
    let ssh = match target {
        hel_targets::TargetTemplate::LocalPodman(_)
        | hel_targets::TargetTemplate::LocalDocker(_) => None,
        hel_targets::TargetTemplate::SshPodman { ssh, .. } => Some(ssh),
        _ => return Vec::new(),
    };
    let overlaid = mounts
        .iter()
        .filter(|mount| !mount.read_only)
        .map(|mount| mount.source.clone())
        .collect::<Vec<_>>();
    if overlaid.is_empty() {
        return Vec::new();
    }
    let filesystems = match hel_targets::probe_filesystem_types(ssh, &overlaid, executor) {
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
        .filter(|mount| !mount.read_only)
        .zip(filesystems)
    {
        let Some(reason) = hel_targets::overlay_unsupported_filesystem(&filesystem) else {
            continue;
        };
        mount.read_only = true;
        notices.push(format!(
            "Mounted {} read-only: the overlay is unreliable on {filesystem} ({reason}).",
            mount.source.display()
        ));
    }
    notices
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
    locator: &hel_targets::TargetLocator,
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

fn inherits_controller_git_settings(locator: &hel_targets::TargetLocator) -> bool {
    !matches!(
        locator,
        hel_targets::TargetLocator::LocalBare { .. } | hel_targets::TargetLocator::SshBare { .. }
    )
}

fn inherited_git_setting_commands(
    locator: &hel_targets::TargetLocator,
    session_id: &str,
    settings: BTreeMap<String, String>,
) -> Result<Vec<CommandSpec>> {
    if matches!(locator, hel_targets::TargetLocator::SshBare { .. }) {
        return Ok(Vec::new());
    }
    settings
        .into_iter()
        .map(|(key, value)| {
            hel_targets::command_on_locator(
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

    use hel::hel_config::ProjectRepository;
    use hel::hel_state::{HelState, SessionRecord, SessionState, TargetLocator};
    use hel::hel_targets::{
        self, AdditionalMount, ContainerTemplate, ProjectBundleSpec, SshTarget,
    };

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

    fn podman_target() -> hel_targets::TargetTemplate {
        hel_targets::TargetTemplate::LocalPodman(ContainerTemplate {
            image: "ubuntu:24.04".into(),
            pull_policy: Default::default(),
            extra_run_args: Vec::new(),
            workspace_storage: Default::default(),
        })
    }

    fn probe_bundle() -> ProjectBundleSpec {
        ProjectBundleSpec {
            primary: "app".into(),
            repositories: vec![hel::hel_targets::RepositorySpec {
                url: Some("https://github.com/example/app.git".into()),
                destination: "app".into(),
                git_ref: None,
                reference: None,
            }],
        }
    }

    #[test]
    fn a_source_that_cannot_overlay_is_mounted_read_only_and_reported() {
        let executor = ProbeExecutor::answering("nfs");
        let mut mounts = vec![AdditionalMount {
            source: PathBuf::from("/nfs/share"),
            destination: PathBuf::from("/mnt/share"),
            read_only: false,
        }];

        let notices = enforce_overlay_capable_mounts(&podman_target(), &mut mounts, &executor);

        assert!(mounts[0].read_only);
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0]
                .contains("Mounted /nfs/share read-only: the overlay is unreliable on nfs (network filesystem)"),
            "{notices:?}"
        );
        let plan = hel_targets::provision_plan(
            &podman_target(),
            "0123456789abcdef0123456789abcdef",
            &probe_bundle(),
            &mounts,
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
            read_only: false,
        }];

        let notices = enforce_overlay_capable_mounts(&podman_target(), &mut mounts, &executor);

        assert!(!mounts[0].read_only);
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].contains("keep the copy-on-write overlay")
                && notices[0].contains("cannot read file system information"),
            "{notices:?}"
        );
        let plan = hel_targets::provision_plan(
            &podman_target(),
            "0123456789abcdef0123456789abcdef",
            &probe_bundle(),
            &mounts,
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
            read_only: false,
        }];
        for target in [
            hel_targets::TargetTemplate::AppleContainer(ContainerTemplate {
                image: "ubuntu:24.04".into(),
                pull_policy: Default::default(),
                extra_run_args: Vec::new(),
                workspace_storage: Default::default(),
            }),
            hel_targets::TargetTemplate::AwsEc2(hel_targets::AwsTemplate {
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
            assert!(!mounts[0].read_only);
        }
    }

    /// A mount the user already marked read-only has no overlay to protect, so
    /// the probe never has to reach a host that may not answer.
    #[test]
    fn mounts_already_read_only_are_not_probed() {
        struct UnusedExecutor;

        impl CommandExecutor for UnusedExecutor {
            fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
                panic!("a read-only mount must not probe: {}", command.program)
            }
        }

        let mut mounts = vec![AdditionalMount {
            source: PathBuf::from("/host/cache"),
            destination: PathBuf::from("/mnt/cache"),
            read_only: true,
        }];

        assert!(
            enforce_overlay_capable_mounts(&podman_target(), &mut mounts, &UnusedExecutor)
                .is_empty()
        );
    }

    #[test]
    fn failed_new_session_provisioning_discards_provisional_record() {
        let session_id = "0123456789abcdef0123456789abcdef";
        let record = SessionRecord {
            workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: session_id.into(),
            title: "new session".into(),
            harness_kind: hel::hel_config::HarnessKind::Codex,
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
        let mut state = HelState::default();
        state.sessions.insert(session_id.into(), record);

        let result = apply_new_session_provisioning_result(
            &mut state,
            session_id,
            Err(anyhow::anyhow!("container creation failed")),
        );

        assert!(result.is_err());
        assert!(!state.sessions.contains_key(session_id));
    }
    #[test]
    fn failed_new_worker_start_discards_session_only_after_target_cleanup() {
        let session_id = "0123456789abcdef0123456789abcdef";
        let mut session = SessionRecord {
            workspace_id: hel::hel_workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            archived: false,
            container_cpus: None,
            container_memory: None,
            id: session_id.into(),
            title: "new session".into(),
            harness_kind: hel::hel_config::HarnessKind::Kimi,
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
        let mut cleaned = HelState::default();
        cleaned.sessions.insert(session_id.into(), session.clone());

        let failure =
            apply_failed_new_session_rollback(&mut cleaned, session_id, "ACP startup failed", None);

        assert!(!cleaned.sessions.contains_key(session_id));
        assert!(
            failure
                .to_string()
                .contains("provisional session discarded")
        );

        session.state = SessionState::Disconnected;
        let mut cleanup_failed = HelState::default();
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
            hel_targets::TargetLocator::LocalPodman {
                container_id: "abcdef012345".into(),
                workspace_storage: Default::default(),
            },
            hel_targets::TargetLocator::AppleContainer {
                container_id: "abcdef012346".into(),
            },
            hel_targets::TargetLocator::AwsEc2 {
                profile: "default".into(),
                region: "us-east-1".into(),
                instance_id: "i-1234567890abcdef0".into(),
                ssh: ssh.clone(),
                workspace: ".local/share/hel/workspaces/018f9dd2-a3b4-7c8d-9000-123456789abc"
                    .into(),
            },
            hel_targets::TargetLocator::SshPodman {
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

        let persistent = hel_targets::TargetLocator::SshBare {
            ssh,
            workspace: "/srv/hel/018f9dd2-a3b4-7c8d-9000-123456789abc".into(),
        };
        let local = hel_targets::TargetLocator::LocalBare {
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
        let ssh = hel::hel_config::SshConnection {
            host: "builder".into(),
            user: None,
            identity_file: None,
            extra_args: Vec::new(),
        };
        let guardian = TargetTemplate::SshBare {
            ssh: ssh.clone(),
            permissions: hel::hel_config::PermissionMode::Guardian,
            workspace_prefix: ".local/share/hel/workspaces".into(),
        };
        let podman = TargetTemplate::SshPodman {
            ssh: ssh.clone(),
            container: hel::hel_config::ContainerTemplate {
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
            permissions: hel::hel_config::PermissionMode::Yolo,
            workspace_prefix: ".local/share/hel/workspaces".into(),
        };

        assert_eq!(
            TargetTemplate::LocalBare.execution_policy(),
            hel::hel_config::ExecutionPolicy::ConfiguredApprovals
        );
        assert_eq!(
            guardian.execution_policy(),
            hel::hel_config::ExecutionPolicy::ConfiguredApprovals
        );
        assert_eq!(
            podman.execution_policy(),
            hel::hel_config::ExecutionPolicy::Unconstrained
        );
        assert_eq!(
            yolo.execution_policy(),
            hel::hel_config::ExecutionPolicy::Unconstrained
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

    fn container_targets() -> Vec<hel_targets::TargetTemplate> {
        let container = ContainerTemplate {
            image: "ubuntu:24.04".into(),
            pull_policy: Default::default(),
            extra_run_args: Vec::new(),
            workspace_storage: Default::default(),
        };
        vec![
            hel_targets::TargetTemplate::LocalPodman(container.clone()),
            hel_targets::TargetTemplate::AppleContainer(container.clone()),
            hel_targets::TargetTemplate::SshPodman {
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
        let name = hel_targets::resource_name(PROVISIONED_SESSION).unwrap();
        for target in container_targets() {
            let plan =
                hel_targets::provision_plan(&target, PROVISIONED_SESSION, &probe_bundle(), &[])
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
        let plan = hel_targets::provision_plan(&target, PROVISIONED_SESSION, &probe_bundle(), &[])
            .unwrap();
        let executor = RecordingExecutor::succeeding();

        let (_, repositories) =
            provision_target_creation(&plan, &target, PROVISIONED_SESSION, &executor, |_| {
                Ok(TargetLocator::LocalPodman {
                    container_id: hel_targets::resource_name(PROVISIONED_SESSION)?,
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
            let plan =
                hel_targets::provision_plan(&target, PROVISIONED_SESSION, &probe_bundle(), &[])
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
        let plan = hel_targets::provision_plan(&target, PROVISIONED_SESSION, &probe_bundle(), &[])
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
        let target = hel_targets::TargetTemplate::LocalBare;
        let plan =
            hel_targets::provision_bare_project_plan(&target, PROVISIONED_SESSION, "/srv/project")
                .unwrap();
        let executor = RecordingExecutor::succeeding();

        let error = provision_target(&plan, &target, PROVISIONED_SESSION, &executor, |_| {
            bail!("the worker root was unreadable")
        })
        .unwrap_err();

        assert!(!format!("{error:#}").contains("cleanup"));
        assert!(executor.commands().is_empty());
    }

    #[test]
    fn a_broker_that_keeps_stopping_is_restarted_a_bounded_number_of_times() {
        let mut runs = 0;
        let mut waits = Vec::new();

        let error = supervise_broker_restarts(
            3,
            || true,
            || {
                runs += 1;
                Ok((Duration::from_millis(1), "signal: 9".into()))
            },
            |delay| waits.push(delay),
        )
        .unwrap_err();

        // The first run, then one restart per attempt.
        assert_eq!(runs, 4);
        assert_eq!(waits.len(), 3);
        assert!(waits[0] < waits[2], "{waits:?}");
        assert!(
            format!("{error:#}").contains("stopped 4 times in a row"),
            "{error:#}"
        );
    }

    #[test]
    fn a_broker_another_controller_took_over_is_left_alone() {
        let mut runs = 0;

        supervise_broker_restarts(
            3,
            || false,
            || {
                runs += 1;
                Ok((Duration::from_millis(1), "exit status: 0".into()))
            },
            |_| unreachable!("a broker that needs no restart must not be waited on"),
        )
        .unwrap();

        assert_eq!(runs, 1);
    }

    /// A broker that served the session for a while before dying starts a
    /// fresh restart budget, so one bad hour never exhausts a session.
    #[test]
    fn a_broker_that_ran_healthily_earns_a_fresh_restart_budget() {
        let mut runs = 0;

        let error = supervise_broker_restarts(
            2,
            || true,
            || {
                runs += 1;
                Ok(if runs <= 4 {
                    (BROKER_HEALTHY_RUN, "signal: 9".into())
                } else {
                    (Duration::from_millis(1), "signal: 9".into())
                })
            },
            |_| (),
        )
        .unwrap_err();

        assert_eq!(runs, 6);
        assert!(format!("{error:#}").contains("stopped 3 times in a row"));
    }

    /// Turns a re-executed copy of the stop test into a stand-in for a running
    /// broker. Holding the slot's lock is exactly what makes a process this
    /// session's broker, so a stand-in that holds it is indistinguishable from
    /// the real thing to everything that has to stop one.
    #[cfg(unix)]
    const BROKER_STAND_IN_PID_PATH: &str = "MJ_TEST_BROKER_STAND_IN_PID_PATH";

    fn retirable_broker_files(directory: &Path) -> BrokerFiles {
        let files = BrokerFiles::in_directory(directory, PROVISIONED_SESSION);
        GitBrokerSpec {
            session_id: PROVISIONED_SESSION.into(),
            bridge: CommandSpec::new("true", Vec::<String>::new()),
            repositories: BTreeMap::new(),
            ready_path: files.ready.clone(),
            pid_path: files.pid.clone(),
        }
        .write(&files.spec)
        .unwrap();
        std::fs::write(&files.ready, "ready\n").unwrap();
        std::fs::write(&files.log, "broker log\n").unwrap();
        files
    }

    /// A closing session stops its broker on purpose: the process goes, the
    /// supervisor that was keeping it alive returns quietly, and the log keeps
    /// what it had without a word about a lost origin.
    #[cfg(unix)]
    #[test]
    fn retiring_a_session_stops_its_running_broker_and_reports_nothing() {
        if let Some(pid_path) = std::env::var_os(BROKER_STAND_IN_PID_PATH) {
            let _slot = crate::hel_git_proxy::claim_broker_pid_file(Path::new(&pid_path)).unwrap();
            // Retirement is what ends this process; the sleep only bounds the
            // damage when it fails to.
            std::thread::sleep(Duration::from_secs(60));
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let files = retirable_broker_files(directory.path());
        let test_name = format!(
            "{}::retiring_a_session_stops_its_running_broker_and_reports_nothing",
            module_path!()
                .strip_prefix("mj_controller::")
                .unwrap_or(module_path!())
        );
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", &test_name, "--nocapture"])
            .env(BROKER_STAND_IN_PID_PATH, &files.pid)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        {
            use std::os::unix::process::CommandExt;
            // Brokers lead their own process group, and stopping one signals
            // that group.
            command.process_group(0);
        }
        let child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !broker_is_alive(&files.pid) {
            assert!(
                Instant::now() < deadline,
                "the stand-in broker never claimed its slot"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let (finished, supervised) = std::sync::mpsc::channel();
        let supervisor = {
            let files = files.clone();
            std::thread::spawn(move || {
                supervise_git_broker(PROVISIONED_SESSION, &files, child);
                let _ = finished.send(());
            })
        };

        retire_broker_files(&files).unwrap();

        supervised
            .recv_timeout(Duration::from_secs(30))
            .expect("the retired broker's supervisor never finished");
        supervisor.join().unwrap();
        assert!(!broker_is_alive(&files.pid));
        assert!(!files.spec.exists());
        assert!(!files.pid.exists());
        assert!(!files.ready.exists());
        assert_eq!(std::fs::read_to_string(&files.log).unwrap(), "broker log\n");
    }

    /// The same stop that ends a broker also ends its restarts: a broker that
    /// died is started again, a broker its session retired is not.
    #[test]
    fn a_retired_broker_is_never_restarted_where_a_dead_one_is() {
        let directory = tempfile::tempdir().unwrap();
        let files = retirable_broker_files(directory.path());
        // A PID file whose broker is gone: the death was unexpected.
        std::fs::write(&files.pid, "424242").unwrap();
        assert!(broker_needs_restart(&files));

        retire_broker_files(&files).unwrap();

        assert!(!broker_needs_restart(&files));
        let mut runs = 0;
        supervise_broker_restarts(
            BROKER_RESTART_ATTEMPTS,
            || broker_needs_restart(&files),
            || {
                runs += 1;
                Ok((Duration::from_millis(1), "signal: 15".into()))
            },
            |_| unreachable!("a retired broker must never be waited on for a restart"),
        )
        .unwrap();

        // The broker that was already running, and not one restart after it.
        assert_eq!(runs, 1);
        assert!(!files.spec.exists());
        assert!(!files.pid.exists());
        assert!(!files.ready.exists());
        assert_eq!(std::fs::read_to_string(&files.log).unwrap(), "broker log\n");
    }

    /// A session with no local repositories never had a broker, so retiring it
    /// touches nothing at all.
    #[test]
    fn retiring_a_session_that_never_had_a_broker_creates_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let brokers = directory.path().join("git-brokers");

        retire_broker_files(&BrokerFiles::in_directory(&brokers, PROVISIONED_SESSION)).unwrap();

        assert!(!brokers.exists());
        assert!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn a_converting_resume_seeds_from_its_own_checkout() {
        let repository = ProjectRepository {
            id: "project".into(),
            github: None,
            local: Some(PathBuf::from("/home/dev/project")),
            destination: PathBuf::from("project"),
            git_ref: None,
        };
        let configured = PathBuf::from("/home/dev/project");
        let missing = vec![(&repository, &configured)];
        let checkout = PathBuf::from("/home/dev/project/.mj/worktrees/session");

        assert_eq!(seed_sources(&missing, &LocalBootstrap::Skip), None);
        assert_eq!(
            seed_sources(&missing, &LocalBootstrap::Seed)
                .unwrap()
                .into_iter()
                .map(|(_, source)| source.clone())
                .collect::<Vec<_>>(),
            vec![configured.clone()]
        );
        assert_eq!(
            seed_sources(&missing, &LocalBootstrap::SeedFrom(checkout.clone()))
                .unwrap()
                .into_iter()
                .map(|(_, source)| source.clone())
                .collect::<Vec<_>>(),
            vec![checkout]
        );
    }
}

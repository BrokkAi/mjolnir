//! Orphan-worker discovery, adoption, and destruction.

use mj_core::targets::SshTrafficClass;
use std::path::{Path, PathBuf};

use crate::targets::managed_sessions_from_container_json;

use anyhow::{Context, Result, bail};

use crate::session_manager::StandaloneSession;
use mj_core::config::{AwsAddressSource, SshConnection, TargetTemplate};
use mj_core::state::{PodmanWorkspaceLocator, SessionRecord, SessionState, TargetLocator};

use crate::targets::{self, CommandExecutor, CommandOutput, CommandSpec, SshTarget};
use mj_core::worker_launch::WorkerOwnership;

use super::backend::{ContainerOverrides, backend_locator, backend_target};
use super::{Controller, now};

pub use mj_core::state::{RecoveryCandidate, RecoveryScan};

pub struct AdoptionControl {
    pub cancelled: tokio_util::sync::CancellationToken,
    pub progress: Option<tokio::sync::watch::Sender<Option<Vec<String>>>>,
    pub installed: Option<tokio::sync::oneshot::Sender<AdoptionPublication>>,
    pub lifecycle_gate: std::sync::Arc<crate::recovery_gate::RecoveryGate>,
}

pub struct AdoptionPublication {
    pub admission: AdoptionInstall,
    pub acknowledged: tokio::sync::oneshot::Sender<()>,
}

pub struct AdoptionOutcome {
    pub result: Result<()>,
    /// Bounded failure or connected-state publication owns the final reply.
    /// Earlier installation transfers its holders with AdoptionPublication.
    pub admission: Option<AdoptionInstall>,
}

pub struct AdoptionInstall {
    _session: crate::recovery_gate::ForegroundAdmission,
    _handoff: crate::upgrade::Work,
}

impl Controller {
    /// Find managed resources which are not represented by the controller's
    /// current state. Labels/tags establish Hel ownership; the worker marker
    /// supplies profile and bundle metadata when it is available.
    ///
    /// Unless `all_instances` is set, only workers stamped with this
    /// instance's identity are listed: a QA instance sharing a host with a
    /// production instance must never see the production workers as its own.
    pub fn scan_orphan_workers(
        &self,
        executor: &impl CommandExecutor,
        all_instances: bool,
    ) -> RecoveryScan {
        let mut scan = RecoveryScan {
            instance_id: mj_core::config::instance_identity(),
            ..RecoveryScan::default()
        };
        let moves = match crate::database::load_move_operations() {
            Ok(moves) => moves,
            Err(error) => {
                scan.warnings
                    .push(format!("cannot read Move resource ownership: {error:#}"));
                return scan;
            }
        };
        for (target_id, template) in &self.config.targets {
            match scan_target_workers(target_id, template, executor) {
                Ok(candidates) => {
                    for mut candidate in candidates {
                        let move_owned = moves.iter().any(|op| {
                            op.selection.session_id == candidate.session_id
                                && op.prepared_destination.as_ref().is_some_and(|d| {
                                    d.owns_resource()
                                        && match &candidate.locator {
                                            mj_core::state::TargetLocator::AwsEc2 {
                                                instance_id,
                                                ..
                                            } => d
                                                .instance_id()
                                                .is_none_or(|owned| owned == instance_id),
                                            _ => false,
                                        }
                                })
                        });
                        if move_owned || self.state_represents(&candidate) {
                            continue;
                        }
                        candidate.tracked_session = self
                            .state
                            .sessions
                            .get(&candidate.session_id)
                            .map(|record| record.state);
                        scan.candidates.push(candidate);
                    }
                }
                Err(error) => scan.warnings.push(format!("target {target_id}: {error:#}")),
            }
        }
        scan.candidates.sort_by(|left, right| {
            (&left.session_id, &left.target_template_id)
                .cmp(&(&right.session_id, &right.target_template_id))
        });
        scan.candidates.dedup_by(|left, right| {
            left.session_id == right.session_id
                && left.target_template_id == right.target_template_id
        });
        if !all_instances {
            restrict_to_instance(&mut scan);
        }
        scan
    }

    /// Whether the controller's own state still stands for this resource, so
    /// recovery must leave it alone.
    ///
    /// A session stands for a resource only while some session record holds
    /// the locator that names it; every record is checked, because a sub-agent
    /// child borrows its parent's container and keeps the owner alive. Sharing
    /// the session id is not enough: a failed provision and a failed move both
    /// clear the locator and leave the session in `error`, and such a record
    /// has no way of its own to reach the resource it left running.
    ///
    /// While the controller is provisioning or tearing a session down it is
    /// writing that locator, so a record in one of those states is taken as
    /// representing its resource whatever the locator says at this instant.
    fn state_represents(&self, candidate: &RecoveryCandidate) -> bool {
        if self
            .state
            .sessions
            .get(&candidate.session_id)
            .is_some_and(|record| locator_in_flight(record.state))
        {
            return true;
        }
        self.state.sessions.values().any(|record| {
            record
                .target
                .as_ref()
                .is_some_and(|locator| same_resource(locator, &candidate.locator))
        })
    }

    #[cfg(test)]
    pub async fn adopt_orphan_worker(
        &mut self,
        session_id: &str,
        target_id: &str,
        profile_override: Option<&str>,
        bundle_override: Option<&str>,
        all_instances: bool,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        let request = mj_client::daemon::RecoveryAdoptionRequest {
            session_id: session_id.to_owned(),
            target_id: target_id.to_owned(),
            profile: profile_override.map(str::to_owned),
            bundle: bundle_override.map(str::to_owned),
            all_instances,
        };
        self.adopt_orphan_worker_controlled(
            &request,
            executor,
            AdoptionControl {
                cancelled: tokio_util::sync::CancellationToken::new(),
                progress: None,
                installed: None,
                lifecycle_gate: std::sync::Arc::new(crate::recovery_gate::RecoveryGate::default()),
            },
        )
        .await
        .result
    }

    pub async fn adopt_orphan_worker_controlled(
        &mut self,
        request: &mj_client::daemon::RecoveryAdoptionRequest,
        executor: &(impl CommandExecutor + Sync),
        mut control: AdoptionControl,
    ) -> AdoptionOutcome {
        let session_id = request.session_id.as_str();
        let mut admission = None;
        let mut failure_owner = None;
        let mut result = self
            .adopt_orphan_worker_inner(
                request,
                executor,
                &mut control,
                &mut admission,
                &mut failure_owner,
            )
            .await;
        if result.is_err()
            && admission.is_none()
            && !control.cancelled.is_cancelled()
            && !crate::upgrade::is_draining()
            && let Some(expected) = failure_owner.as_ref()
        {
            let cancelled = std::sync::atomic::AtomicBool::new(false);
            let owner = tokio::select! {
                biased;
                _ = control.cancelled.cancelled() => None,
                _ = crate::upgrade::gate().wait_until_draining() => None,
                owner = control.lifecycle_gate.enter_foreground(session_id, "adoption failure", crate::recovery_gate::LifecycleOrigin::Person, &cancelled, executor) => Some(owner),
            };
            if let Some(owner) = owner {
                match owner.and_then(|owner| {
                    crate::upgrade::activity_unless_draining("adoption install")
                        .map(|work| (owner, work))
                }) {
                    Ok((owner, work)) => {
                        admission = Some(AdoptionInstall {
                            _session: owner,
                            _handoff: work,
                        });
                        result = Err(mj_core::runtime::off_async_worker(|| {
                            self.record_adoption_failure(expected, result.err().unwrap())
                        }));
                    }
                    Err(error) => {
                        result = result.map_err(|failure| {
                            failure.context(format!("could not retain adoption failure: {error:#}"))
                        })
                    }
                }
            }
        }
        AdoptionOutcome { result, admission }
    }

    async fn adopt_orphan_worker_inner(
        &mut self,
        request: &mj_client::daemon::RecoveryAdoptionRequest,
        executor: &(impl CommandExecutor + Sync),
        control: &mut AdoptionControl,
        install_admission: &mut Option<AdoptionInstall>,
        failure_owner: &mut Option<SessionRecord>,
    ) -> Result<()> {
        let session_id = request.session_id.as_str();
        let target_id = request.target_id.as_str();
        let profile_override = request.profile.as_deref();
        let bundle_override = request.bundle.as_deref();
        let all_instances = request.all_instances;
        let (record, newly_adopted) = mj_core::runtime::off_async_worker(|| -> Result<_> {
            Ok(match self.state.sessions.get(session_id).cloned() {
                // Adoption records its session before projection attachment, so a
                // failed attachment leaves a tracked session that never connected. That
                // record is the one to finish, not a reason to refuse the retry.
                Some(existing) if adoption_unfinished(&existing, target_id) => {
                    for (flag, requested, adopted) in [
                        ("profile", profile_override, existing.last_profile.as_str()),
                        ("bundle", bundle_override, existing.bundle_id.as_str()),
                    ] {
                        if let Some(requested) = requested
                            && requested != adopted
                        {
                            bail!(
                                "session {session_id} was already adopted with {flag} {adopted:?}; retry without --{flag}"
                            );
                        }
                    }
                    // Only a request that passed the target/profile/bundle checks
                    // can publish a diagnostic on this provisional resource owner.
                    *failure_owner = Some(existing.clone());
                    (existing, false)
                }
                // A leftover resource whose session id is still taken cannot be
                // adopted: the id would have to name two sessions. Destroying it
                // is the whole of what recovery can offer for such a resource.
                Some(existing) => bail!(
                    "session {session_id} is already tracked in state {}; use `recover destroy` to remove a resource it left behind",
                    existing.state.as_str()
                ),
                None => {
                    let scan = self.scan_orphan_workers(executor, true);
                    let candidate = scan
                        .candidates
                        .into_iter()
                        .find(|candidate| {
                            candidate.session_id == session_id
                                && candidate.target_template_id == target_id
                        })
                        .with_context(|| {
                            format!(
                                "no managed orphan {session_id} was found on target {target_id:?}"
                            )
                        })?;
                    require_instance_access(&candidate, &scan.instance_id, all_instances)?;
                    let profile_id = profile_override
                        .map(str::to_owned)
                        .or_else(|| {
                            candidate
                                .ownership
                                .as_ref()
                                .map(|marker| marker.profile_id.clone())
                        })
                        .context("orphan has no ownership marker; pass --profile")?;
                    let bundle_id = bundle_override
                        .map(str::to_owned)
                        .or_else(|| {
                            candidate
                                .ownership
                                .as_ref()
                                .map(|marker| marker.bundle_id.clone())
                        })
                        .context("orphan has no ownership marker; pass --bundle")?;
                    let profile = self
                        .config
                        .profiles
                        .get(&profile_id)
                        .with_context(|| format!("unknown profile {profile_id:?}"))?;
                    let (bundle_id, project) =
                        if let Some(saved) = crate::database::saved_project(&bundle_id)? {
                            saved
                        } else {
                            let bundle = self
                                .config
                                .bundles
                                .get(&bundle_id)
                                .with_context(|| format!("unknown project {bundle_id:?}"))?;
                            (
                                bundle_id,
                                crate::project_catalog::snapshot(bundle, executor, false)?,
                            )
                        };
                    let workspace_id = candidate
                        .ownership
                        .as_ref()
                        .map(|ownership| ownership.workspace_id.clone())
                        .unwrap_or_else(|| mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned());
                    let container_workspace = self
                        .config
                        .targets
                        .get(target_id)
                        .and_then(|template| {
                            recovery_backend_locator(template, &candidate.locator, session_id).ok()
                        })
                        .and_then(|backend| {
                            adopted_container_workspace(&backend, session_id, executor)
                        });
                    let no_github_credentials = candidate
                        .ownership
                        .as_ref()
                        .map_or(!self.config.github.session_credentials, |ownership| {
                            ownership.no_github_credentials
                        });
                    let mut record = adopted_session_record(
                        session_id,
                        target_id,
                        profile_id,
                        profile.kind,
                        bundle_id,
                        workspace_id,
                        candidate.locator,
                    );
                    record.target_runtime = Some(
                        record
                            .target_runtime_settings(&self.config)?
                            .into_owned()
                            .into(),
                    );
                    record.no_github_credentials = no_github_credentials;
                    record.container_workspace = container_workspace;
                    record.project = Some(project.into());
                    (record, true)
                }
            })
        })?;
        let locator = record
            .target
            .as_ref()
            .context("adopted session has no target locator")?;
        let spec = mj_core::runtime::off_async_worker(|| -> Result<_> {
            let backend = backend_locator(locator, &record, &self.config)?;
            targets::reconnect_plan(&backend, session_id)?
                .commands
                .into_iter()
                .next()
                .context("reconnect plan is empty")
        })?;
        let mut client = tokio::select! {
            biased;
            _ = control.cancelled.cancelled() => bail!("adoption cancelled; the worker continues running; rerun recover adopt"),
            _ = crate::upgrade::gate().wait_until_draining() => bail!("adoption cancelled for daemon handoff; rerun recover adopt"),
            client = StandaloneSession::prepare_adoption(&spec, session_id) => client.context("orphan relay could not attach")?,
        };
        let operational = tokio::select! {
            biased;
            _ = control.cancelled.cancelled() => bail!("adoption cancelled; the worker continues running"),
            _ = crate::upgrade::gate().wait_until_draining() => bail!("adoption cancelled for daemon handoff"),
            state = client.status() => state?,
        };
        ensure_adoptable_worker(&operational)?;
        client.set_adoption_wait_progress(control.progress.clone());
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let needs_cut = tokio::select! {
            biased;
            _ = control.cancelled.cancelled() => bail!("adoption cancelled; worker continues running"),
            _ = crate::upgrade::gate().wait_until_draining() => bail!("adoption cancelled for daemon handoff; rerun recover adopt"),
            result = StandaloneSession::adoption_needs_cut(&mut client, session_id) => result,
        };
        let baseline = match needs_cut {
            Ok(false) => None,
            Ok(true) => Some(tokio::select! {
                biased;
                _ = control.cancelled.cancelled() => bail!("adoption cancelled while waiting; the worker continues running; rerun recover adopt"),
                _ = crate::upgrade::gate().wait_until_draining() => bail!("adoption cancelled for daemon handoff while waiting; rerun recover adopt"),
                result = client.live_session_baseline() => result?,
            }),
            Err(error) => {
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<crate::worker_client::LiveBaselineUnsupported>()
                        .is_some()
                }) {
                    let needs_all_instances = mj_core::runtime::off_async_worker(|| {
                        self.config
                            .targets
                            .get(target_id)
                            .and_then(|template| {
                                scan_target_workers(target_id, template, executor).ok()
                            })
                            .and_then(|candidates| {
                                candidates
                                    .into_iter()
                                    .find(|candidate| same_resource(locator, &candidate.locator))
                            })
                            .and_then(|candidate| candidate.instance_id)
                            .as_deref()
                            != Some(mj_core::config::instance_identity().as_str())
                    });
                    let error =
                        unsupported_orphan_adoption(session_id, target_id, needs_all_instances);
                    if !newly_adopted {
                        let owner = tokio::select! {
                            biased;
                            _ = control.cancelled.cancelled() => bail!("adoption cancelled"),
                            _ = crate::upgrade::gate().wait_until_draining() => bail!("adoption cancelled for daemon handoff"),
                            owner = control.lifecycle_gate.enter_foreground(session_id, "adoption refusal", crate::recovery_gate::LifecycleOrigin::Person, &cancelled, executor) => owner?,
                        };
                        *install_admission = Some(AdoptionInstall {
                            _session: owner,
                            _handoff: crate::upgrade::activity_unless_draining("adoption install")?,
                        });
                        mj_core::runtime::off_async_worker(|| {
                            self.revalidate_adoption_resource(&record)
                        })?;
                        let current = mj_core::runtime::off_async_worker(|| {
                            crate::database::refuse_adoption_if_current(
                                &record,
                                &format!("orphan adoption failed: {error:#}"),
                                &now(),
                            )
                        })?;
                        self.state.sessions.insert(session_id.to_owned(), current);
                    }
                    return Err(error);
                }
                return Err(error);
            }
        };
        let cut = if let Some(baseline) = baseline {
            tokio::select! {
                biased;
                _ = control.cancelled.cancelled() => bail!("adoption cancelled; cut discarded"),
                _ = crate::upgrade::gate().wait_until_draining() => bail!("daemon handoff began; cut discarded; rerun recover adopt"),
                result = StandaloneSession::cache_baseline_attachments(&mut client, &baseline) => result?,
            }
            let projection =
                mj_transcript::projection::materialized_session_from_live_baseline(&baseline)?;
            Some((projection, baseline))
        } else {
            None
        };
        #[cfg(any(test, feature = "test-hooks", feature = "test-support"))]
        tokio::task::spawn_blocking(|| {
            mj_core::test_hooks::reach_test_hook("adoption_cut_before_install")
        })
        .await
        .context("adoption install hook failed")??;
        // The wait owns no handoff admission. The cut is discarded if a drain
        // or another lifecycle won before this single install decision.
        let install_owner = tokio::select! {
            biased;
            _ = control.cancelled.cancelled() => bail!("adoption cancelled before install; cut discarded"),
            _ = crate::upgrade::gate().wait_until_draining() => bail!("daemon handoff began before adoption install; cut discarded; rerun recover adopt"),
            owner = control.lifecycle_gate.enter_foreground(session_id, "adoption install", crate::recovery_gate::LifecycleOrigin::Person, &cancelled, executor) => owner?,
        };
        mj_core::runtime::off_async_worker(|| self.revalidate_adoption_resource(&record))?;
        let operational = tokio::select! {
            biased;
            _ = control.cancelled.cancelled() => bail!("adoption cancelled before install; cut discarded"),
            _ = crate::upgrade::gate().wait_until_draining() => bail!("daemon handoff began during the worker probe; cut discarded; rerun recover adopt"),
            status = client.status() => status.context("worker disappeared before adoption install; cut discarded")?,
        };
        ensure_adoptable_worker(&operational)?;
        // No remote work runs under this owner. A rename either sees this new
        // row in its migration, or completes first and invalidates this lookup.
        let (current_config, config_owner) = tokio::select! {
            biased;
            _ = control.cancelled.cancelled() => bail!("adoption cancelled before configuration validation"),
            _ = crate::upgrade::gate().wait_until_draining() => bail!("daemon handoff began before configuration validation"),
            config = tokio::task::spawn_blocking(mj_core::config::Config::load_locked) => config.context("read current adoption configuration")??,
        };
        anyhow::ensure!(
            current_config.profiles.contains_key(&record.last_profile),
            "profile {:?} is no longer configured; worker continues running; retry recover adopt with --profile selecting a configured profile",
            record.last_profile
        );
        anyhow::ensure!(
            current_config
                .targets
                .contains_key(&record.target_template_id),
            "target template {:?} is no longer configured; worker continues running; retry recover adopt selecting its current target template",
            record.target_template_id
        );
        let guard = AdoptionInstall {
            _session: install_owner,
            _handoff: crate::upgrade::activity_unless_draining("adoption install")?,
        };
        let accepted = tokio::select! {
            biased;
            _ = control.cancelled.cancelled() => bail!("adoption cancelled before writer acceptance; cut discarded"),
            _ = crate::upgrade::gate().wait_until_draining() => bail!("daemon handoff began before writer acceptance; cut discarded; rerun recover adopt"),
            accepted = crate::database::enqueue_adopted_session(record.clone(), cut, now(), (guard, config_owner)) => accepted?,
        };
        let (result, (guard, config_owner)) = accepted.finish().await?;
        drop(config_owner);
        let current = match result {
            Ok(current) => current,
            Err(error) => {
                *install_admission = Some(guard);
                return Err(error);
            }
        };
        self.state
            .sessions
            .insert(session_id.to_owned(), current.clone());
        *failure_owner = Some(current.clone());
        self.finish_adoption_attachment(
            (&spec, client),
            current,
            guard,
            executor,
            control,
            install_admission,
        )
        .await
        .context("worker adopted; reattaching failed")?;
        Ok(())
    }

    async fn finish_adoption_attachment(
        &mut self,
        prepared: (&CommandSpec, crate::worker_client::RelayClient),
        expected: SessionRecord,
        guard: AdoptionInstall,
        executor: &(impl CommandExecutor + Sync),
        control: &mut AdoptionControl,
        publication_admission: &mut Option<AdoptionInstall>,
    ) -> Result<()> {
        let (spec, client) = prepared;
        let session_id = expected.id.clone();
        let installed = control.installed.take();
        if let Some(installed) = installed {
            let (acknowledged, notice) = tokio::sync::oneshot::channel();
            if let Err(publication) = installed.send(AdoptionPublication {
                admission: guard,
                acknowledged,
            }) {
                drop(publication);
                bail!("worker adopted; reattaching interrupted before installed notice");
            }
            notice
                .await
                .context("worker adopted; reattaching interrupted before installed notice")?;
        } else {
            drop(guard);
        }
        client
            .detach()
            .await
            .context("worker adopted; could not close preparation connection")?;
        let attach = async {
            let ready = async {
                let mut connection = StandaloneSession::connect_command(spec, &session_id).await?;
                ensure_adoptable_worker(connection.operational())?;
                // This shared wait retains the controller-owned startup deadline
                // for pre-66 workers; current workers decide their own expiry.
                super::readiness::wait_for_native_session(
                    &mut connection,
                    executor,
                    expected.harness_kind,
                )
                .await?;
                ensure_adoptable_worker(connection.operational())?;
                Ok::<_, anyhow::Error>(connection)
            };
            tokio::select! {
                biased;
                _ = control.cancelled.cancelled() => bail!("attachment interrupted"),
                _ = crate::upgrade::gate().wait_until_draining() => bail!("attachment interrupted for daemon handoff"),
                connection = ready => connection,
            }
        };
        let connection = attach.await.context("worker adopted; reattaching failed")?;
        let native = connection.operational().native_session_id.clone();
        // Close this ordinary connection before another actor attaches.
        connection
            .detach()
            .await
            .context("worker adopted; detaching attachment probe failed")?;
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let owner = tokio::select! {
            biased;
            _ = control.cancelled.cancelled() => bail!("worker adopted; reattaching interrupted before publication"),
            _ = crate::upgrade::gate().wait_until_draining() => bail!("worker adopted; reattaching interrupted for daemon handoff"),
            owner = control.lifecycle_gate.enter_foreground(&session_id, "adoption connected publication", crate::recovery_gate::LifecycleOrigin::Person, &cancelled, executor) => owner?,
        };
        let guard = AdoptionInstall {
            _session: owner,
            _handoff: crate::upgrade::activity_unless_draining("adoption connected publication")?,
        };
        let accepted = tokio::select! {
            biased;
            _ = control.cancelled.cancelled() => bail!("worker adopted; reattaching interrupted before publication"),
            _ = crate::upgrade::gate().wait_until_draining() => bail!("worker adopted; reattaching interrupted for daemon handoff"),
            accepted = crate::database::enqueue_adoption_connected(expected, native, now(), guard) => accepted?,
        };
        let (result, guard) = accepted.finish().await?;
        // Acceptance owns the definitive reply even when its writer rejects a
        // stale publication. Handoff cannot race a second admission attempt.
        *publication_admission = Some(guard);
        let current = result?;
        self.state.sessions.insert(session_id.to_owned(), current);
        // The short connected-publication holder spans runtime refresh and
        // the final reply; ordinary catch-up above owns no lifecycle holder.
        Ok(())
    }

    fn revalidate_adoption_resource(&mut self, expected: &SessionRecord) -> Result<()> {
        let state = crate::database::load_state()?;
        let locator = expected
            .target
            .as_ref()
            .context("adoption has no worker locator")?;
        for current in state.sessions.values() {
            if current
                .target
                .as_ref()
                .is_some_and(|target| same_resource(target, locator))
            {
                anyhow::ensure!(
                    current.id == expected.id
                        && adoption_unfinished(current, &expected.target_template_id)
                        && current.target == expected.target
                        && current.last_profile == expected.last_profile
                        && current.bundle_id == expected.bundle_id,
                    "worker is already named by session {} in state {}; adoption cut discarded",
                    current.id,
                    current.state.as_str()
                );
            }
        }
        if let Some(current) = state.sessions.get(&expected.id) {
            anyhow::ensure!(
                adoption_unfinished(current, &expected.target_template_id)
                    && current.target == expected.target,
                "session {} changed before adoption; cut discarded",
                expected.id
            );
        }
        self.state = state;
        Ok(())
    }

    fn record_adoption_failure(
        &mut self,
        expected: &SessionRecord,
        error: anyhow::Error,
    ) -> anyhow::Error {
        match crate::database::record_adoption_failure_if_current(
            expected,
            &format!("orphan adoption failed: {error:#}"),
            &now(),
        ) {
            Ok(Some(current)) => {
                self.state.sessions.insert(expected.id.clone(), current);
                error
            }
            Ok(None) => {
                self.state.sessions.remove(&expected.id);
                error
            }
            Err(persist_error) => error.context(format!(
                "could not record the adoption failure: {persist_error:#}"
            )),
        }
    }

    pub fn destroy_orphan_worker(
        &self,
        session_id: &str,
        target_id: &str,
        confirmation: &str,
        all_instances: bool,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<()> {
        if confirmation != session_id {
            bail!("refusing destructive recovery: --confirm must exactly match the session ID");
        }
        let scan = self.scan_orphan_workers(executor, true);
        let candidate = scan
            .candidates
            .into_iter()
            .find(|candidate| {
                candidate.session_id == session_id && candidate.target_template_id == target_id
            })
            .with_context(|| {
                format!("no managed orphan {session_id} was found on target {target_id:?}")
            })?;
        require_instance_access(&candidate, &scan.instance_id, all_instances)?;
        let template = self.config.targets.get(target_id).unwrap();
        let backend = recovery_backend_locator(template, &candidate.locator, session_id)?;
        crate::worker_lifecycle::run_blocking(
            session_id,
            "destroy recovered environment",
            executor,
            || {
                let environments = crate::database::worker_environments(session_id)?;
                let owned = environments
                    .into_iter()
                    .filter(|environment| {
                        environment.owner_session_id == session_id
                            && environment
                                .placement
                                .target
                                .as_ref()
                                .is_some_and(|target| target.same_resource(&candidate.locator))
                    })
                    .collect::<Vec<_>>();
                if owned.is_empty() {
                    let root = targets::worker_root(&backend, session_id)?;
                    super::worker_binary::stop_worker(
                        &crate::worker_lifecycle::require(session_id)?,
                        executor,
                        &backend,
                        &root,
                    )?;
                    executor.notify_notice("Worker stopped. Files were retained because complete shared-environment ownership is unavailable.");
                    return Ok(());
                }
                for environment in owned {
                    super::environment::retire_owned_environment(
                        environment,
                        &self.config,
                        executor,
                        super::BranchDisposition::Keep,
                        super::CheckoutDisposition::Remove,
                    )?;
                }
                Ok(())
            },
        )
    }
}

fn unsupported_orphan_adoption(
    session_id: &str,
    target: &str,
    all_instances: bool,
) -> anyhow::Error {
    anyhow::anyhow!(
        "the worker's history no longer reaches its start, and it predates live-cut recovery, so it cannot be adopted. The agent's work continues on the worker, and its harness session files remain on the target. When you are done, run: {}",
        recovery_destroy_command(session_id, target, all_instances)
    )
}

/// Choose advice from the current durable owner, including provisional retries.
pub fn adoption_destroy_command(
    session_id: &str,
    target: &str,
    all_instances: bool,
) -> Result<String> {
    if crate::database::read_durable_session_record(session_id)?
        .is_some_and(|record| record.target.is_some() && record.target_template_id == target)
    {
        Ok(format!(
            "{} destroy --session {}",
            recovery_cli_prefix(),
            mj_core::targets::posix_quote(session_id)
        ))
    } else {
        Ok(recovery_destroy_command(session_id, target, all_instances))
    }
}

fn recovery_cli_prefix() -> String {
    let instance = mj_core::config::instance_name()
        .map(|name| format!(" --instance {}", mj_core::targets::posix_quote(&name)))
        .unwrap_or_default();
    format!("mj{instance}")
}

pub fn recovery_destroy_command(session_id: &str, target: &str, all_instances: bool) -> String {
    let id = mj_core::targets::posix_quote(session_id);
    let target = mj_core::targets::posix_quote(target);
    let scope = if all_instances {
        " --all-instances"
    } else {
        ""
    };
    format!(
        "{} recover destroy --session {id} --target {target} --confirm {id}{scope}",
        recovery_cli_prefix()
    )
}

/// States in which the controller is itself writing the session's locator.
/// The record cannot be read as settled evidence about its resource then.
const fn locator_in_flight(state: SessionState) -> bool {
    matches!(
        state,
        SessionState::Provisioning
            | SessionState::Checkpointing
            | SessionState::Closing
            | SessionState::Destroying
    )
}

/// Whether two locators name the same managed resource. Only the identity
/// fields count: borrowing, workspace storage, and a discovered address say
/// how a session reaches the resource, not which resource it is.
pub(super) fn same_resource(left: &TargetLocator, right: &TargetLocator) -> bool {
    left.same_resource(right)
}

/// Drop candidates another or an unknown instance created, counting them so
/// the user learns that `--all-instances` would show more.
fn restrict_to_instance(scan: &mut RecoveryScan) {
    let before = scan.candidates.len();
    scan.candidates
        .retain(|candidate| candidate.instance_id.as_deref() == Some(scan.instance_id.as_str()));
    scan.hidden_other_instances = before - scan.candidates.len();
}

/// Refuse to act on a worker that another instance created, or whose
/// instance is unknown, unless the caller widened the scope explicitly.
fn require_instance_access(
    candidate: &RecoveryCandidate,
    scan_instance: &str,
    all_instances: bool,
) -> Result<()> {
    if all_instances {
        return Ok(());
    }
    match candidate.instance_id.as_deref() {
        Some(instance) if instance == scan_instance => Ok(()),
        Some(other) => bail!(
            "worker {} belongs to instance {other:?}, not this instance {scan_instance:?}; pass --all-instances to act on it",
            candidate.session_id
        ),
        None => bail!(
            "worker {} has no instance stamp (created by an older build); pass --all-instances to act on it",
            candidate.session_id
        ),
    }
}

fn ensure_adoptable_worker(state: &mj_core::relay::RelayOperationalState) -> Result<()> {
    if let Some(mj_core::relay::HarnessPreparation::Failed { step, error, .. }) =
        &state.harness_preparation
    {
        bail!("worker has no runnable harness: preparation failed at {step}: {error}");
    }
    anyhow::ensure!(
        !state.checkpoint_only
            && !matches!(
                state.execution,
                mj_core::relay::RelayExecutionState::Closing
                    | mj_core::relay::RelayExecutionState::Closed
            ),
        "worker has no runnable harness (checkpoint-only or closed relay); recover its saved checkpoint or destroy the leftover worker"
    );
    // A preparing worker may acquire its native identity after attachment.
    Ok(())
}

/// Worker markers can outlive the controller database that created them. Keep
/// a workspace marker only when this controller knows its identity; otherwise
/// group the adopted worker under one durable recovery workspace.
#[cfg(test)]
fn resolve_recovery_workspace_id(marked_workspace_id: &str) -> Result<String> {
    if marked_workspace_id == mj_core::workspace::DEFAULT_WORKSPACE_ID {
        return Ok(marked_workspace_id.to_owned());
    }
    if crate::database::list_workspaces()?
        .into_iter()
        .any(|workspace| workspace.id == marked_workspace_id)
    {
        return Ok(marked_workspace_id.to_owned());
    }
    Ok(crate::database::create_or_get_workspace("Recovered")?.id)
}

/// The workspace a running container actually holds. A container created
/// before per-session workspaces has only the shared `/workspace`, so an
/// adoption that cannot see `/workspace/<session id>` keeps the legacy path
/// rather than pointing the recovered harness at a directory that is not there.
fn adopted_container_workspace(
    backend: &targets::TargetLocator,
    session_id: &str,
    executor: &impl CommandExecutor,
) -> Option<PathBuf> {
    if !matches!(
        backend,
        targets::TargetLocator::LocalPodman { .. }
            | targets::TargetLocator::LocalDocker { .. }
            | targets::TargetLocator::AppleContainer { .. }
            | targets::TargetLocator::SshPodman { .. }
            | targets::TargetLocator::SshDocker { .. }
            | targets::TargetLocator::SshAppleContainer { .. }
            | targets::TargetLocator::SshNitro { .. }
    ) {
        return None;
    }
    let workspace = targets::new_container_workspace(session_id).ok()?;
    let command = targets::command_on_locator(
        SshTrafficClass::Control,
        backend,
        session_id,
        vec![
            "test".to_owned(),
            "-d".to_owned(),
            workspace.to_string_lossy().into_owned(),
        ],
        "probe the adopted session workspace",
    )
    .ok()?;
    match executor.execute(&command) {
        Ok(output) if output.status == 0 => Some(workspace),
        Ok(_) => None,
        Err(error) => {
            tracing::debug!(
                session_id,
                %error,
                "could not probe the adopted session workspace; assuming the shared one"
            );
            None
        }
    }
}

/// The session record adoption commits before it tries the relay handshake.
fn adopted_session_record(
    session_id: &str,
    target_id: &str,
    profile_id: String,
    harness_kind: mj_core::config::HarnessKind,
    bundle_id: String,
    workspace_id: String,
    locator: TargetLocator,
) -> SessionRecord {
    let now = now();
    SessionRecord {
        oracle_choice: None,
        oracle: true,
        notifications_ignored: false,
        project: None,
        target_runtime: None,
        launch_base: None,
        launch_branch: None,
        checkout: None,
        publication: None,
        build_cache: None,
        subagents: None,
        // The adopting caller probes the running container for this.
        container_workspace: None,
        create_managed_worktree: None,
        workspace_id,
        archived: false,
        id: session_id.to_owned(),
        title: format!("Recovered {}", &session_id[..session_id.len().min(8)]),
        harness_kind,
        last_profile: profile_id,
        bundle_id,
        project_directory: None,
        managed_worktree: None,
        review: None,
        no_project_memory: false,
        no_github_events: false,
        no_github_credentials: false,
        target_template_id: target_id.to_owned(),
        resource_allocation: None,
        additional_mounts: Vec::new(),
        state: SessionState::Disconnected,
        target: Some(Box::new(locator)),
        native_session_id: None,
        acp_session_title: None,
        session_title_override: None,
        created_at: now.clone(),
        updated_at: now,
        viewed_through_event_ordinal: 0,
        draft_input: String::new(),
        last_error: None,
        dismissed_failure: None,
        failure_generation: 0,
        last_checkpoint_error: None,
        checkpoint: None,
    }
}

/// Whether a tracked session is one an adoption committed and never finished:
/// it names this target, carries the locator the scan found, and no harness
/// session has ever been observed on it. Such a record is the retry, so
/// adoption completes it instead of refusing it as already tracked.
fn adoption_unfinished(record: &SessionRecord, target_id: &str) -> bool {
    record.state == SessionState::Disconnected
        && record.native_session_id.is_none()
        && record.target_template_id == target_id
        && record.target.is_some()
}

fn scan_target_workers(
    target_id: &str,
    template: &TargetTemplate,
    executor: &impl CommandExecutor,
) -> Result<Vec<RecoveryCandidate>> {
    let mut candidates = match template {
        // Local bare sessions persist their locator in the controller database.
        // Do not infer an adoptable project from Hel's transient worker directory.
        TargetTemplate::LocalBare => Vec::new(),
        TargetTemplate::LocalPodman { .. } => scan_container_engine(
            target_id,
            template,
            "podman",
            vec![
                "ps".into(),
                "--all".into(),
                "--filter".into(),
                format!("label={}=true", targets::MANAGED_LABEL),
                "--format".into(),
                "json".into(),
            ],
            executor,
        )?,
        TargetTemplate::LocalDocker { .. } => scan_container_engine(
            target_id,
            template,
            "docker",
            vec![
                "ps".into(),
                "--all".into(),
                "--filter".into(),
                format!("label={}=true", targets::MANAGED_LABEL),
                "--format".into(),
                "json".into(),
            ],
            executor,
        )?,
        TargetTemplate::AppleContainer { .. } => scan_container_engine(
            target_id,
            template,
            "container",
            vec![
                "list".into(),
                "--all".into(),
                "--format".into(),
                "json".into(),
            ],
            executor,
        )?,
        TargetTemplate::SshPodman { ssh, .. } => {
            let remote = vec![
                "podman".into(),
                "ps".into(),
                "--all".into(),
                "--filter".into(),
                format!("label={}=true", targets::MANAGED_LABEL),
                "--format".into(),
                "json".into(),
            ];
            let output = execute_scan(
                executor,
                ssh_spec(ssh, remote),
                "scan remote Podman workers",
            )?;
            candidates_from_container_json(target_id, template, &output.stdout)?
        }
        TargetTemplate::SshDocker { ssh, .. } => {
            let remote = vec![
                "docker".into(),
                "ps".into(),
                "--all".into(),
                "--filter".into(),
                format!("label={}=true", targets::MANAGED_LABEL),
                "--format".into(),
                "json".into(),
            ];
            let output = execute_scan(
                executor,
                ssh_spec(ssh, remote),
                "scan remote Docker workers",
            )?;
            candidates_from_container_json(target_id, template, &output.stdout)?
        }
        TargetTemplate::SshNitro { ssh, .. } => {
            // The engine lists sandboxes as Podman lists containers: `ps`
            // with a label filter, a JSON array with a `Labels` object.
            let remote = targets::with_engine_argv(vec![
                targets::MICROVM_ENGINE.into(),
                "ps".into(),
                "--all".into(),
                "--filter".into(),
                format!("label={}=true", targets::MANAGED_LABEL),
                "--format".into(),
                "json".into(),
            ]);
            let output = execute_scan(
                executor,
                ssh_spec(ssh, remote),
                "scan remote Nitro sandbox workers",
            )?;
            candidates_from_container_json(target_id, template, &output.stdout)?
        }
        TargetTemplate::SshAppleContainer { ssh, .. } => {
            let remote = vec![
                "container".into(),
                "list".into(),
                "--all".into(),
                "--format".into(),
                "json".into(),
            ];
            let output = execute_scan(
                executor,
                ssh_spec(ssh, remote),
                "scan remote Apple container workers",
            )?;
            candidates_from_container_json(target_id, template, &output.stdout)?
        }
        TargetTemplate::AwsEc2 {
            aws_profile,
            region,
            address_source,
            ..
        } => {
            let profile = aws_profile.clone().unwrap_or_else(|| "default".into());
            let output = execute_scan(
                executor,
                CommandSpec::new(
                    "aws",
                    [
                        "--profile".into(),
                        profile,
                        "--region".into(),
                        region.clone(),
                        "ec2".into(),
                        "describe-instances".into(),
                        "--filters".into(),
                        format!("Name=tag:{},Values=true", targets::MANAGED_TAG),
                        "Name=instance-state-name,Values=pending,running,stopping,stopped".into(),
                        "--output".into(),
                        "json".into(),
                    ],
                )
                .purpose("scan managed EC2 workers"),
                "scan managed EC2 workers",
            )?;
            candidates_from_aws_json(target_id, address_source.clone(), &output.stdout)?
        }
        TargetTemplate::SshBare { ssh, .. } => {
            let output = execute_scan(
                executor,
                ssh_spec(
                    ssh,
                    vec![
                        "find".into(),
                        ".local/share/hel/workers".into(),
                        "-mindepth".into(),
                        "2".into(),
                        "-maxdepth".into(),
                        "2".into(),
                        "-name".into(),
                        "ownership.json".into(),
                        "-print".into(),
                    ],
                ),
                "scan bare SSH worker markers",
            )?;
            output
                .stdout
                .split(|byte| *byte == b'\n')
                .filter_map(|line| {
                    let path = match std::str::from_utf8(line) {
                        Ok(path) => path.trim(),
                        Err(error) => {
                            tracing::debug!(%error, "recovery scan skipped a non-UTF-8 worker marker path");
                            return None;
                        }
                    };
                    let Some(session_id) = Path::new(path)
                        .parent()
                        .and_then(|parent| parent.file_name())
                        .and_then(|name| name.to_str())
                    else {
                        tracing::debug!(path, "recovery scan skipped a malformed worker marker path");
                        return None;
                    };
                    if let Err(error) = targets::resource_name(session_id) {
                        tracing::debug!(session_id, %error, "recovery scan skipped an invalid session id");
                        return None;
                    }
                    let backend = match backend_target(template, None, ContainerOverrides::default()) {
                        Ok(backend) => backend,
                        Err(error) => {
                            tracing::debug!(session_id, %error, "recovery scan could not construct the target backend");
                            return None;
                        }
                    };
                    let workspace = match targets::workspace_for(&backend, session_id) {
                        Ok(workspace) => workspace,
                        Err(error) => {
                            tracing::debug!(session_id, %error, "recovery scan could not derive the target workspace");
                            return None;
                        }
                    };
                    Some(RecoveryCandidate {
                        session_id: session_id.to_owned(),
                        target_template_id: target_id.to_owned(),
                        locator: TargetLocator::SshBare {
                            host: ssh.host.clone(),
                            workspace: PathBuf::from(workspace),
                            // The scan reads the session id from the worker
                            // directory name and `targets::worker_root` falls
                            // back to it, so the root still resolves.
                            worker_id: None,
                        },
                        ownership: None,
                        instance_id: None,
                        tracked_session: None,
                    })
                })
                .collect()
        }
    };
    for candidate in &mut candidates {
        candidate.ownership = read_recovery_ownership(template, candidate, executor);
        // The label is authoritative; the marker only fills in when the
        // resource carries no stamp (bare SSH workers have no labels at all).
        if candidate.instance_id.is_none() {
            candidate.instance_id = candidate
                .ownership
                .as_ref()
                .and_then(|marker| marker.instance_id.clone());
        }
    }
    Ok(candidates)
}

fn scan_container_engine(
    target_id: &str,
    template: &TargetTemplate,
    engine: &str,
    args: Vec<String>,
    executor: &impl CommandExecutor,
) -> Result<Vec<RecoveryCandidate>> {
    let output = execute_scan(
        executor,
        CommandSpec::new(engine, args).purpose("scan managed container workers"),
        "scan managed container workers",
    )?;
    candidates_from_container_json(target_id, template, &output.stdout)
}

/// Where a Podman session's workspace really lives, derived the same way
/// provisioning derives it: the backend container template plus the session id.
fn recovery_workspace_storage(
    template: &TargetTemplate,
    session_id: &str,
) -> Result<PodmanWorkspaceLocator> {
    let backend = backend_target(template, None, ContainerOverrides::default())?;
    let container = match &backend {
        targets::TargetTemplate::LocalPodman(container) => container,
        targets::TargetTemplate::SshPodman { container, .. } => container,
        _ => bail!("target template is not a Podman target"),
    };
    Ok(PodmanWorkspaceLocator::from(
        targets::podman_workspace_locator(container, session_id)?,
    ))
}

fn candidates_from_container_json(
    target_id: &str,
    template: &TargetTemplate,
    stdout: &[u8],
) -> Result<Vec<RecoveryCandidate>> {
    let sessions = managed_sessions_from_container_json(stdout)?;
    Ok(sessions
        .into_iter()
        .filter_map(|(session_id, instance_id)| {
            let generated = match targets::resource_name(&session_id) {
                Ok(generated) => generated,
                Err(error) => {
                    tracing::debug!(%session_id, %error, "recovery scan skipped an invalid managed session id");
                    return None;
                }
            };
            // A Podman workspace may live in a volume or a host directory, and
            // destroying the container alone would leave it behind. The storage
            // follows from the target template and the session id, so derive it
            // here rather than recording the container layer by default.
            let workspace_storage = match template {
                TargetTemplate::LocalPodman { .. } | TargetTemplate::SshPodman { .. } => {
                    match recovery_workspace_storage(template, &session_id) {
                        Ok(storage) => storage,
                        Err(error) => {
                            tracing::debug!(%session_id, %error, "recovery scan could not derive the Podman workspace storage");
                            return None;
                        }
                    }
                }
                _ => Default::default(),
            };
            let locator = match template {
                TargetTemplate::LocalPodman { .. } => TargetLocator::LocalPodman {
                    borrowed_from: None,
                    container_id: generated,
                    workspace_storage,
                },
                TargetTemplate::LocalDocker { .. } => TargetLocator::LocalDocker {
                    borrowed_from: None,
                    container_id: generated,
                },
                TargetTemplate::AppleContainer { .. } => TargetLocator::AppleContainer {
                    borrowed_from: None,
                    container_id: generated,
                },
                TargetTemplate::SshPodman { ssh, .. } => TargetLocator::SshPodman {
                    borrowed_from: None,
                    host: ssh.host.clone(),
                    container_id: generated,
                    workspace_storage,
                },
                TargetTemplate::SshDocker { ssh, .. } => TargetLocator::SshDocker {
                    borrowed_from: None,
                    host: ssh.host.clone(),
                    container_id: generated,
                },
                TargetTemplate::SshAppleContainer { ssh, .. } => {
                    TargetLocator::SshAppleContainer {
                        borrowed_from: None,
                        host: ssh.host.clone(),
                        container_id: generated,
                    }
                }
                TargetTemplate::SshNitro { ssh, .. } => TargetLocator::SshNitro {
                    borrowed_from: None,
                    host: ssh.host.clone(),
                    container_id: generated,
                },
                _ => return None,
            };
            Some(RecoveryCandidate {
                session_id,
                target_template_id: target_id.to_owned(),
                locator,
                ownership: None,
                instance_id,
                tracked_session: None,
            })
        })
        .collect())
}

fn candidates_from_aws_json(
    target_id: &str,
    address_source: AwsAddressSource,
    stdout: &[u8],
) -> Result<Vec<RecoveryCandidate>> {
    let value: serde_json::Value =
        serde_json::from_slice(stdout).context("parse AWS instance JSON")?;
    let mut result = Vec::new();
    let reservations = value
        .get("Reservations")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    for instance in reservations.iter().flat_map(|reservation| {
        reservation
            .get("Instances")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
    }) {
        let tags = instance
            .get("Tags")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let tag = |key: &str| {
            tags.iter()
                .find(|tag| tag.get("Key").and_then(serde_json::Value::as_str) == Some(key))
                .and_then(|tag| tag.get("Value"))
                .and_then(serde_json::Value::as_str)
        };
        if tag(targets::MANAGED_TAG) != Some("true") {
            continue;
        }
        let Some(session_id) = tag(targets::SESSION_TAG).map(str::to_owned) else {
            continue;
        };
        targets::resource_name(&session_id)?;
        let instance_id = instance
            .get("InstanceId")
            .and_then(serde_json::Value::as_str)
            .context("managed EC2 instance omitted InstanceId")?
            .to_owned();
        let field = match address_source {
            AwsAddressSource::PublicDns => "PublicDnsName",
            AwsAddressSource::PublicIp => "PublicIpAddress",
            AwsAddressSource::PrivateDns => "PrivateDnsName",
            AwsAddressSource::PrivateIp => "PrivateIpAddress",
        };
        let address = instance
            .get(field)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let created_by = tag(targets::INSTANCE_TAG).map(str::to_owned);
        result.push(RecoveryCandidate {
            session_id,
            target_template_id: target_id.to_owned(),
            locator: TargetLocator::AwsEc2 {
                instance_id,
                address,
            },
            ownership: None,
            instance_id: created_by,
            tracked_session: None,
        });
    }
    Ok(result)
}

fn execute_scan(
    executor: &impl CommandExecutor,
    command: CommandSpec,
    operation: &str,
) -> Result<CommandOutput> {
    let output = executor.execute(&command)?;
    if output.status != 0 {
        bail!(
            "{operation} failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

fn ssh_spec(ssh: &SshConnection, remote: Vec<String>) -> CommandSpec {
    targets::ssh_command_owned(SshTrafficClass::Control, &SshTarget::from(ssh), remote)
}

fn read_recovery_ownership(
    template: &TargetTemplate,
    candidate: &RecoveryCandidate,
    executor: &impl CommandExecutor,
) -> Option<WorkerOwnership> {
    let backend =
        match recovery_backend_locator(template, &candidate.locator, &candidate.session_id) {
            Ok(backend) => backend,
            Err(error) => {
                tracing::debug!(
                    session_id = %candidate.session_id,
                    %error,
                    "could not construct a recovery ownership probe"
                );
                return None;
            }
        };
    let root = match targets::worker_root(&backend, &candidate.session_id) {
        Ok(root) => root,
        Err(error) => {
            tracing::debug!(
                session_id = %candidate.session_id,
                %error,
                "could not derive a recovery worker root"
            );
            return None;
        }
    };
    let command = match targets::command_on_locator(
        SshTrafficClass::Control,
        &backend,
        &candidate.session_id,
        vec!["cat".into(), format!("{root}/ownership.json")],
        "read worker ownership marker",
    ) {
        Ok(command) => command,
        Err(error) => {
            tracing::debug!(
                session_id = %candidate.session_id,
                %error,
                "could not construct a recovery ownership command"
            );
            return None;
        }
    };
    let output = match executor.execute(&command) {
        Ok(output) => output,
        Err(error) => {
            tracing::debug!(
                session_id = %candidate.session_id,
                %error,
                "could not read a recovery worker ownership marker"
            );
            return None;
        }
    };
    if output.status != 0 {
        tracing::debug!(
            session_id = %candidate.session_id,
            status = output.status,
            "recovery worker ownership probe returned a failure"
        );
        return None;
    }
    let marker: WorkerOwnership = match serde_json::from_slice(&output.stdout) {
        Ok(marker) => marker,
        Err(error) => {
            tracing::debug!(
                session_id = %candidate.session_id,
                %error,
                "recovery worker ownership marker was not valid JSON"
            );
            return None;
        }
    };
    if !(1..=WorkerOwnership::VERSION).contains(&marker.version)
        || marker.session_id != candidate.session_id
        || marker.target_template_id != candidate.target_template_id
    {
        tracing::debug!(
            session_id = %candidate.session_id,
            marker_session_id = %marker.session_id,
            marker_target_template_id = %marker.target_template_id,
            "recovery worker ownership marker did not match the candidate"
        );
        return None;
    }
    Some(marker)
}

/// The backend locator recovery uses to destroy an orphan worker.
///
/// This stays separate from `backend_locator` (and from the shared
/// `TryFrom<StoredTarget>` conversion) because destroying an orphan has to
/// succeed in two cases the shared conversion rejects or cannot answer:
///
/// - The AWS arm substitutes `unavailable.invalid` for a missing address on
///   purpose. AWS cleanup terminates the instance through the `aws` CLI
///   (`targets/cleanup.rs`), so an instance with no address must still be
///   destroyable, whereas the shared conversion errors on the missing address.
/// - `worker_id: None` for `SshBare` is correct here. The scan takes the
///   session id from the worker directory name and `targets::worker_root`
///   falls back to the session id, so the root resolves. A borrowed target's
///   shared parent workspace is not discoverable from a scan, and recomputing
///   the child's own (nonexistent) path is the safe direction for destroy.
fn recovery_backend_locator(
    template: &TargetTemplate,
    locator: &TargetLocator,
    session_id: &str,
) -> Result<targets::TargetLocator> {
    Ok(match (template, locator) {
        (TargetTemplate::LocalBare, TargetLocator::LocalBare { worker_root }) => {
            targets::TargetLocator::LocalBare {
                worker_root: worker_root.to_string_lossy().into_owned(),
            }
        }
        (
            TargetTemplate::LocalPodman { .. },
            TargetLocator::LocalPodman {
                container_id,
                workspace_storage,
                ..
            },
        ) => targets::TargetLocator::LocalPodman {
            borrowed_from: None,
            container_id: container_id.clone(),
            workspace_storage: workspace_storage.into(),
        },
        (TargetTemplate::LocalDocker { .. }, TargetLocator::LocalDocker { container_id, .. }) => {
            targets::TargetLocator::LocalDocker {
                borrowed_from: None,
                container_id: container_id.clone(),
            }
        }
        (
            TargetTemplate::AppleContainer { .. },
            TargetLocator::AppleContainer { container_id, .. },
        ) => targets::TargetLocator::AppleContainer {
            borrowed_from: None,
            container_id: container_id.clone(),
        },
        (
            TargetTemplate::SshAppleContainer { ssh, .. },
            TargetLocator::SshAppleContainer {
                host, container_id, ..
            },
        ) => {
            if host != &ssh.host {
                bail!("recovery SSH Apple container host does not match target template")
            }
            targets::TargetLocator::SshAppleContainer {
                borrowed_from: None,
                ssh: SshTarget::from(ssh),
                container_id: container_id.clone(),
            }
        }
        (
            TargetTemplate::SshNitro { ssh, .. },
            TargetLocator::SshNitro {
                host, container_id, ..
            },
        ) => {
            if host != &ssh.host {
                bail!("recovery SSH Nitro host does not match target template")
            }
            targets::TargetLocator::SshNitro {
                borrowed_from: None,
                ssh: SshTarget::from(ssh),
                container_id: container_id.clone(),
            }
        }
        (
            TargetTemplate::SshPodman { ssh, .. },
            TargetLocator::SshPodman {
                container_id,
                workspace_storage,
                ..
            },
        ) => targets::TargetLocator::SshPodman {
            borrowed_from: None,
            ssh: SshTarget::from(ssh),
            container_id: container_id.clone(),
            workspace_storage: workspace_storage.into(),
        },
        (
            TargetTemplate::SshDocker { ssh, .. },
            TargetLocator::SshDocker {
                host, container_id, ..
            },
        ) => {
            if host != &ssh.host {
                bail!("recovery SSH Docker host does not match target template")
            }
            targets::TargetLocator::SshDocker {
                borrowed_from: None,
                ssh: SshTarget::from(ssh),
                container_id: container_id.clone(),
            }
        }
        (TargetTemplate::SshBare { ssh, .. }, TargetLocator::SshBare { workspace, .. }) => {
            targets::TargetLocator::SshBare {
                ssh: SshTarget::from(ssh),
                workspace: workspace.to_string_lossy().into_owned(),
                worker_id: None,
            }
        }
        (
            TargetTemplate::AwsEc2 {
                aws_profile,
                region,
                ssh_user,
                identity_file,
                ssh_args,
                ..
            },
            TargetLocator::AwsEc2 {
                instance_id,
                address,
            },
        ) => targets::TargetLocator::AwsEc2 {
            profile: aws_profile.clone().unwrap_or_else(|| "default".into()),
            region: region.clone(),
            instance_id: instance_id.clone(),
            ssh: SshTarget {
                destination: format!(
                    "{ssh_user}@{}",
                    address.as_deref().unwrap_or("unavailable.invalid")
                ),
                ssh_args: targets::ssh_args_with_identity(ssh_args, identity_file.as_deref()),
            },
            workspace: format!(".local/share/hel/workspaces/{session_id}"),
        },
        _ => bail!("recovery target locator does not match target template"),
    })
}

#[cfg(test)]
mod tests {

    use crate::controller::test_support::{IsolatedTest, test_name};
    use mj_core::config::{
        AwsAddressSource, Config, ContainerTemplate as ConfigContainer, HarnessKind,
        PodmanWorkspaceStorage, TargetTemplate,
    };
    use mj_core::state::{State, TargetLocator};

    use crate::targets::ProcessExecutor;

    use super::*;

    const FAILED_ADOPTION_CHILD: &str = "MJ_TEST_FAILED_ADOPTION_CHILD";

    // Hard-won: 91163d55: failed orphan adoption became non-retryable instead of preserving ownership
    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    async fn a_failed_adoption_records_the_failure_and_stays_retryable() {
        // MJ_DATA_DIR is process-global, so the database-backed half runs in
        // an exact child test with its own data directory.
        if std::env::var_os(FAILED_ADOPTION_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "a_failed_adoption_records_the_failure_and_stays_retryable",
            ))
            .env(FAILED_ADOPTION_CHILD, "1")
            .env("MJ_DATA_DIR", directory.path())
            .run();
            return;
        }
        // Alone in this child process, so it installs the one writer.
        let _writer = crate::database::install_isolated_test_writer();

        let session_id = "0123456789abcdef0123456789abcdef";
        let workers = tempfile::tempdir().unwrap();
        // Exactly what an adoption commits before its handshake, on a worker
        // root that holds no worker binary, so the handshake cannot succeed.
        let mut record = adopted_session_record(
            session_id,
            "local-bare",
            "codex".into(),
            HarnessKind::Codex,
            "project".into(),
            mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            TargetLocator::LocalBare {
                worker_root: workers.path().join(session_id),
            },
        );
        assert!(
            adoption_unfinished(&record, "local-bare"),
            "the record adoption commits must be the record adoption can retry"
        );
        record.last_error = Some("the original worker failure".into());
        crate::database::save_session(&record).unwrap();
        let mut config = Config::default();
        config
            .targets
            .insert("local-bare".into(), TargetTemplate::LocalBare);
        let mut state = State::default();
        state.sessions.insert(session_id.to_owned(), record);
        let mut controller = Controller { config, state };

        // Hard-won: 3210bf3: validation failures overwrote a session the request never owned.
        let unchanged = crate::database::read_durable_session_record(session_id)
            .unwrap()
            .unwrap();
        for (target, profile, bundle) in [
            ("different-target", None, None),
            ("local-bare", Some("different-profile"), None),
            ("local-bare", None, Some("different-bundle")),
        ] {
            controller
                .adopt_orphan_worker(session_id, target, profile, bundle, false, &ProcessExecutor)
                .await
                .unwrap_err();
            assert_eq!(
                crate::database::read_durable_session_record(session_id)
                    .unwrap()
                    .unwrap(),
                unchanged
            );
            assert!(crate::upgrade::active_labels().is_empty());
        }

        let failure = controller
            .adopt_orphan_worker(
                session_id,
                "local-bare",
                None,
                None,
                false,
                &ProcessExecutor,
            )
            .await
            .expect_err("a worker root without a worker cannot complete the handshake");
        assert!(
            format!("{failure:#}").contains("orphan relay"),
            "unexpected failure: {failure:#}"
        );
        let recorded = controller.state.sessions[session_id]
            .last_error
            .clone()
            .expect("the failed handshake was recorded on the session");
        assert!(
            recorded.contains("orphan adoption failed"),
            "unexpected recorded failure: {recorded}"
        );
        assert_eq!(
            controller.state.sessions[session_id].state,
            SessionState::Disconnected
        );
        let stored = crate::database::load_state().unwrap();
        assert_eq!(
            stored.sessions[session_id].last_error.as_deref(),
            Some(recorded.as_str()),
            "the adoption failure was not persisted"
        );

        let retry = controller
            .adopt_orphan_worker(
                session_id,
                "local-bare",
                None,
                None,
                false,
                &ProcessExecutor,
            )
            .await
            .expect_err("the worker is still unreachable");
        let retry = format!("{retry:#}");
        assert!(
            retry.contains("orphan relay"),
            "adoption did not retry the handshake: {retry}"
        );
        assert!(
            !retry.contains("already tracked"),
            "a session adoption never finished blocked its own retry: {retry}"
        );
    }

    const RECOVERY_WORKSPACE_CHILD: &str = "MJ_TEST_RECOVERY_WORKSPACE_CHILD";

    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    async fn orphan_workspace_ids_are_reconciled_before_adoption_persistence() {
        if std::env::var_os(RECOVERY_WORKSPACE_CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(
                module_path!(),
                "orphan_workspace_ids_are_reconciled_before_adoption_persistence",
            ))
            .env(RECOVERY_WORKSPACE_CHILD, "1")
            .env("MJ_DATA_DIR", directory.path())
            .run();
            return;
        }

        let _writer = crate::database::install_isolated_test_writer();
        let default =
            resolve_recovery_workspace_id(mj_core::workspace::DEFAULT_WORKSPACE_ID).unwrap();
        assert_eq!(default, mj_core::workspace::DEFAULT_WORKSPACE_ID);

        let known = crate::database::create_or_get_workspace("Known").unwrap();
        assert_eq!(resolve_recovery_workspace_id(&known.id).unwrap(), known.id);

        let recovered = resolve_recovery_workspace_id("workspace-from-old-controller").unwrap();
        let repeated = resolve_recovery_workspace_id("another-old-workspace").unwrap();
        assert_eq!(repeated, recovered);
        assert_eq!(
            crate::database::list_workspaces()
                .unwrap()
                .iter()
                .filter(|workspace| workspace.name == "Recovered")
                .count(),
            1
        );

        let session_id = "0123456789abcdef0123456789abcdef";
        let workers = tempfile::tempdir().unwrap();
        let record = adopted_session_record(
            session_id,
            "local-bare",
            "codex".into(),
            HarnessKind::Codex,
            "project".into(),
            recovered.clone(),
            TargetLocator::LocalBare {
                worker_root: workers.path().join(session_id),
            },
        );
        crate::database::save_session(&record).unwrap();
        assert_eq!(
            crate::database::load_state().unwrap().sessions[session_id].workspace_id,
            recovered
        );

        // Adoption has already committed the reconciled workspace. Exercise
        // the post-save handshake path with the same fake used by the retry
        // regression, which leaves and persists a diagnostic on failure.
        let mut state = State::default();
        state.sessions.insert(session_id.to_owned(), record);
        let mut config = Config::default();
        config
            .targets
            .insert("local-bare".into(), TargetTemplate::LocalBare);
        let mut controller = Controller { config, state };
        let failure = controller
            .adopt_orphan_worker(
                session_id,
                "local-bare",
                None,
                None,
                false,
                &ProcessExecutor,
            )
            .await
            .expect_err("a worker root without a worker cannot complete the handshake");
        assert!(
            format!("{failure:#}").contains("orphan relay"),
            "unexpected failure: {failure:#}"
        );
        let stored = crate::database::load_state().unwrap();
        assert_eq!(stored.sessions[session_id].workspace_id, recovered);
        assert!(
            stored.sessions[session_id]
                .last_error
                .as_deref()
                .is_some_and(|error| error.contains("orphan adoption failed"))
        );
    }

    #[test]
    fn a_session_that_completed_its_handshake_is_not_adoptable_again() {
        let mut record = adopted_session_record(
            "0123456789abcdef0123456789abcdef",
            "local-bare",
            "codex".into(),
            HarnessKind::Codex,
            "project".into(),
            mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            TargetLocator::LocalBare {
                worker_root: std::path::PathBuf::from("/workers/0123456789abcdef0123456789abcdef"),
            },
        );
        record.native_session_id = Some("native-session".into());
        assert!(!adoption_unfinished(&record, "local-bare"));

        record.native_session_id = None;
        assert!(
            !adoption_unfinished(&record, "other-target"),
            "a record adopted onto another target is not this target's retry"
        );
    }

    #[test]
    fn recovery_container_scan_requires_both_managed_and_session_labels() {
        let template = TargetTemplate::LocalPodman {
            container: ConfigContainer {
                build_cache: None,
                image: "ignored".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: Default::default(),
                workspace_storage: Default::default(),
            },
        };
        let json = serde_json::json!([
            {"Labels": {"dev.mj.managed": "true", "dev.mj.session": "0123456789abcdef0123456789abcdef", "dev.mj.instance": "qa0916"}},
            {"Labels": {"dev.mj.managed": "false", "dev.mj.session": "not-owned"}},
            {"configuration": {"labels": "dev.mj.managed=true,dev.mj.session=abcdef0123456789abcdef0123456789"}}
        ]);
        let candidates = candidates_from_container_json(
            "local",
            &template,
            serde_json::to_string(&json).unwrap().as_bytes(),
        )
        .unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].session_id, "0123456789abcdef0123456789abcdef");
        assert_eq!(candidates[0].instance_id.as_deref(), Some("qa0916"));
        assert_eq!(
            candidates[1].instance_id, None,
            "a container without an instance label is of unknown origin"
        );
    }

    /// Answers the container listing the scan runs and fails everything else,
    /// which is what a container with no worker in it does to the ownership
    /// probe.
    struct ListingExecutor {
        listing: String,
    }

    impl CommandExecutor for ListingExecutor {
        fn pause_deadline(&self) -> mj_core::targets::DeadlinePause {
            mj_core::targets::DeadlinePause::none()
        }

        fn cancellation_requested_without_deadline(&self) -> bool {
            self.cancellation_requested()
        }

        fn execute_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute(command)
        }

        fn execute_with_stdin_without_deadline(
            &self,
            command: &mj_core::targets::CommandSpec,
            input: &mut (dyn std::io::Read + Send),
        ) -> anyhow::Result<mj_core::targets::CommandOutput> {
            self.execute_with_stdin(command, input)
        }

        fn execute(&self, command: &CommandSpec) -> Result<CommandOutput> {
            if command.program == "podman" && command.args.first().map(String::as_str) == Some("ps")
            {
                return Ok(CommandOutput {
                    status: 0,
                    stdout: self.listing.clone().into_bytes(),
                    stderr: Vec::new(),
                });
            }
            Ok(CommandOutput {
                status: 1,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    fn podman_scan_controller(record: SessionRecord) -> (Controller, ListingExecutor) {
        let mut config = Config::default();
        config.targets.insert(
            "local".into(),
            TargetTemplate::LocalPodman {
                container: ConfigContainer {
                    build_cache: None,
                    image: "ignored".into(),
                    pull_policy: Default::default(),
                    platform: None,
                    cpus: None,
                    memory: None,
                    environment: Default::default(),
                    workspace_storage: Default::default(),
                },
            },
        );
        let listing = serde_json::to_string(&serde_json::json!([
            {"Labels": {"dev.mj.managed": "true", "dev.mj.session": record.id}}
        ]))
        .unwrap();
        let mut state = State::default();
        state.sessions.insert(record.id.clone(), record);
        (Controller { config, state }, ListingExecutor { listing })
    }

    fn errored_podman_session(session_id: &str) -> SessionRecord {
        let mut record = adopted_session_record(
            session_id,
            "local",
            "codex".into(),
            HarnessKind::Codex,
            "project".into(),
            mj_core::workspace::DEFAULT_WORKSPACE_ID.to_owned(),
            TargetLocator::LocalPodman {
                container_id: targets::resource_name(session_id).unwrap(),
                workspace_storage: PodmanWorkspaceLocator::ContainerLayer,
                borrowed_from: None,
            },
        );
        // What a failed provision or a failed move writes: the session is
        // remembered, the locator that could tear its container down is not.
        record.state = SessionState::Error;
        record.target = None;
        record
    }

    fn isolated_scan(name: &str) -> bool {
        if std::env::var_os("MJ_TEST_MOVE_OWNERSHIP_SCAN").is_some() {
            return true;
        }
        let directory = tempfile::tempdir().unwrap();
        IsolatedTest::new(test_name(module_path!(), name))
            .env("MJ_TEST_MOVE_OWNERSHIP_SCAN", "1")
            .isolated_store(directory.path())
            .run();
        false
    }

    // Hard-won: 75136459: recovery missed a container after its errored session stopped naming it
    #[test]
    fn a_container_an_errored_session_no_longer_names_is_an_orphan() {
        if !isolated_scan("a_container_an_errored_session_no_longer_names_is_an_orphan") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        crate::database::save_state(&State::default()).unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let (controller, executor) = podman_scan_controller(errored_podman_session(session_id));

        let scan = controller.scan_orphan_workers(&executor, true);

        let [candidate] = scan.candidates.as_slice() else {
            panic!("the errored session's container was not offered: {scan:?}");
        };
        assert_eq!(candidate.session_id, session_id);
        assert_eq!(
            candidate.tracked_session,
            Some(SessionState::Error),
            "recovery must say the session id is still taken, so only destroy applies"
        );
    }

    #[test]
    fn a_container_a_session_still_names_is_not_an_orphan() {
        if !isolated_scan("a_container_a_session_still_names_is_not_an_orphan") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        crate::database::save_state(&State::default()).unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let mut record = errored_podman_session(session_id);
        // The same failure, except the record kept its locator: the session's
        // own forced destroy still reaches the container, so recovery stays out.
        record.target = Some(Box::new(TargetLocator::LocalPodman {
            container_id: targets::resource_name(session_id).unwrap(),
            workspace_storage: PodmanWorkspaceLocator::ContainerLayer,
            borrowed_from: None,
        }));
        let (controller, executor) = podman_scan_controller(record);

        let scan = controller.scan_orphan_workers(&executor, true);

        assert!(
            scan.candidates.is_empty(),
            "a container the controller can still drive was offered for destruction: {scan:?}"
        );
    }

    #[test]
    fn a_container_being_provisioned_is_not_an_orphan() {
        if !isolated_scan("a_container_being_provisioned_is_not_an_orphan") {
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        crate::database::save_state(&State::default()).unwrap();
        let session_id = "0123456789abcdef0123456789abcdef";
        let mut record = errored_podman_session(session_id);
        // Provisioning writes the locator as it runs, so a scan that catches
        // the window between `podman run` and locator discovery must not
        // mistake the new container for a leftover.
        record.state = SessionState::Provisioning;
        let (controller, executor) = podman_scan_controller(record);

        let scan = controller.scan_orphan_workers(&executor, true);

        assert!(
            scan.candidates.is_empty(),
            "a container still being provisioned was offered for destruction: {scan:?}"
        );
    }

    fn candidate(session_id: &str, instance_id: Option<&str>) -> RecoveryCandidate {
        RecoveryCandidate {
            session_id: session_id.to_owned(),
            target_template_id: "local".to_owned(),
            locator: TargetLocator::LocalDocker {
                container_id: format!("mj-{session_id}"),
                borrowed_from: None,
            },
            ownership: None,
            instance_id: instance_id.map(str::to_owned),
            tracked_session: None,
        }
    }

    // Hard-won: 18de1bb4: default orphan scan exposed resources belonging to other or unstamped instances
    #[test]
    fn default_scan_scope_hides_other_and_unknown_instances() {
        let mut scan = RecoveryScan {
            candidates: vec![
                candidate("mine", Some("qa")),
                candidate("theirs", Some("prod")),
                candidate("legacy", None),
            ],
            instance_id: "qa".to_owned(),
            ..RecoveryScan::default()
        };
        restrict_to_instance(&mut scan);
        assert_eq!(
            scan.candidates
                .iter()
                .map(|candidate| candidate.session_id.as_str())
                .collect::<Vec<_>>(),
            ["mine"]
        );
        assert_eq!(scan.hidden_other_instances, 2);
    }

    // Hard-won: 18de1bb4: destructive recovery acted on another or unstamped instance without opt-in
    #[test]
    fn acting_on_another_or_unknown_instance_requires_the_explicit_flag() {
        require_instance_access(&candidate("mine", Some("qa")), "qa", false).unwrap();

        let other = require_instance_access(&candidate("theirs", Some("prod")), "qa", false)
            .expect_err("another instance's worker is refused by default");
        assert!(
            other.to_string().contains("belongs to instance \"prod\""),
            "{other}"
        );
        require_instance_access(&candidate("theirs", Some("prod")), "qa", true).unwrap();

        let unknown = require_instance_access(&candidate("legacy", None), "qa", false)
            .expect_err("a worker without a stamp is refused by default");
        assert!(
            unknown.to_string().contains("no instance stamp"),
            "{unknown}"
        );
        require_instance_access(&candidate("legacy", None), "qa", true).unwrap();
    }

    // Hard-won: 4f38727b: orphan destruction left the Podman workspace volume behind
    #[test]
    fn a_podman_orphan_destroy_plan_removes_its_workspace_volume() {
        // `destroy_orphan_worker` rescans, so this drives the two steps it runs
        // on the candidate it finds: the scan's locator and the close plan
        // built from it. Those are what carried the container-layer default.
        let session = "0123456789abcdef0123456789abcdef";
        let template = TargetTemplate::LocalPodman {
            container: ConfigContainer {
                image: "ignored".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: Default::default(),
                workspace_storage: PodmanWorkspaceStorage::PodmanVolume,
                build_cache: None,
            },
        };
        let json = serde_json::json!([
            {"Labels": {"dev.mj.managed": "true", "dev.mj.session": session}}
        ]);

        let candidates = candidates_from_container_json(
            "local",
            &template,
            serde_json::to_string(&json).unwrap().as_bytes(),
        )
        .unwrap();

        let [candidate] = candidates.as_slice() else {
            panic!("expected one orphan candidate, got {candidates:?}");
        };
        let volume = format!("{}-workspace", targets::resource_name(session).unwrap());
        assert!(
            matches!(
                &candidate.locator,
                TargetLocator::LocalPodman {
                    workspace_storage: PodmanWorkspaceLocator::Volume { name },
                    ..
                } if name == &volume
            ),
            "candidate locator lost the volume storage: {:?}",
            candidate.locator
        );
        let backend = recovery_backend_locator(&template, &candidate.locator, session).unwrap();
        let plan = targets::close_plan(&backend, session).unwrap();
        assert!(
            plan.scripts().iter().any(|command| {
                command.args.iter().any(|argument| argument == &volume)
                    && command
                        .args
                        .iter()
                        .any(|argument| argument.contains("podman volume rm"))
            }),
            "destroy plan does not remove the workspace volume: {plan:?}"
        );
    }

    #[test]
    fn recovery_docker_scan_accepts_json_lines_and_builds_a_docker_locator() {
        let template = TargetTemplate::LocalDocker {
            container: ConfigContainer {
                build_cache: None,
                image: "ignored".into(),
                pull_policy: Default::default(),
                platform: None,
                cpus: None,
                memory: None,
                environment: Default::default(),
                workspace_storage: Default::default(),
            },
        };
        let session = "0123456789abcdef0123456789abcdef";
        let output = format!(
            "{{\"Labels\":\"dev.mj.managed=true,dev.mj.session={session},dev.mj.instance=abc123\"}}\n{{\"Labels\":\"dev.mj.managed=false,dev.mj.session=ignored\"}}\n"
        );

        let candidates =
            candidates_from_container_json("docker", &template, output.as_bytes()).unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, session);
        assert_eq!(candidates[0].instance_id.as_deref(), Some("abc123"));
        assert!(matches!(
            &candidates[0].locator,
            TargetLocator::LocalDocker { container_id, .. }
                if container_id == &targets::resource_name(session).unwrap()
        ));
    }

    #[test]
    fn recovery_aws_scan_uses_exact_tagged_instance_and_address() {
        let json = serde_json::json!({"Reservations": [{"Instances": [{
            "InstanceId": "i-exact",
            "PrivateIpAddress": "10.0.0.7",
            "Tags": [
                {"Key": "dev.mj.managed", "Value": "true"},
                {"Key": "dev.mj.session", "Value": "0123456789abcdef0123456789abcdef"},
                {"Key": "dev.mj.instance", "Value": "qa0916"}
            ]
        }]}]});
        let candidates = candidates_from_aws_json(
            "aws",
            AwsAddressSource::PrivateIp,
            serde_json::to_string(&json).unwrap().as_bytes(),
        )
        .unwrap();
        assert_eq!(candidates[0].instance_id.as_deref(), Some("qa0916"));
        assert!(matches!(
            &candidates[0].locator,
            TargetLocator::AwsEc2 { instance_id, address }
                if instance_id == "i-exact" && address.as_deref() == Some("10.0.0.7")
        ));
    }
}

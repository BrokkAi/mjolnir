//! Physical teardown belongs to an environment, never to a session's UI row.
use super::Controller;
use crate::targets::CommandSpec;
use anyhow::{Context, Result, bail, ensure};
use mj_core::config::Config;
use mj_core::state::{ManagedWorktree, SessionRecord};
use mj_core::targets::SshTrafficClass;
use std::path::PathBuf;

use super::backend::recorded_backend_locator;
use super::lifecycle::BranchDisposition;
use crate::database::{self, EnvironmentPhase, EnvironmentRecord, EnvironmentWorker};
use crate::targets::{self, CommandExecutor};
use crate::worker_lifecycle::WorkerPermit;

/// Issued after retirement closes admission and every managed Stop completes.
/// Ancestor-first child admissions prevent accepted mutations from
/// racing Stop or physical removal. Fields are private: callers cannot invent proof.
pub(super) struct Reclamation {
    environment: EnvironmentRecord,
    workers: Vec<(EnvironmentWorker, targets::TargetLocator)>,
    _permits: Vec<WorkerPermit>,
}

/// Exact worker whose managed Stop completed under retained launch permits.
pub(crate) struct StoppedWorker<'a> {
    _authority: &'a Reclamation,
    session_id: &'a str,
    target: &'a targets::TargetLocator,
    root: String,
}
impl StoppedWorker<'_> {
    pub fn session_id(&self) -> &str {
        self.session_id
    }
    pub fn target(&self) -> &targets::TargetLocator {
        self.target
    }
    pub fn root(&self) -> &str {
        &self.root
    }
}
impl Reclamation {
    fn stopped_worker<'a>(
        &'a self,
        worker: &'a EnvironmentWorker,
        target: &'a targets::TargetLocator,
    ) -> Result<StoppedWorker<'a>> {
        ensure!(
            self.workers
                .iter()
                .any(|(entry, location)| std::ptr::eq(entry, worker) && location == target),
            "retirement does not cover worker placement"
        );
        Ok(StoppedWorker {
            _authority: self,
            session_id: &worker.session_id,
            target,
            root: targets::worker_root(target, &worker.session_id)?,
        })
    }
}

fn backend(worker: &EnvironmentWorker, config: &Config) -> Result<targets::TargetLocator> {
    recorded_backend_locator(
        &worker.target,
        &worker.session_id,
        &worker.target_template_id,
        worker.runtime.as_ref(),
        config,
    )
}

impl Reclamation {
    pub fn acquire(
        admission: &crate::recovery_gate::SessionAdmission,
        environment: EnvironmentRecord,
        config: &Config,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<Self> {
        // The physical owner is always first, including environments with no
        // workers yet. All entry points share this ordering with lifecycle.
        let mut permits = vec![WorkerPermit::acquire(
            &environment.owner_session_id,
            admission,
        )?];
        let gate = admission.gate_and_session().0;
        let (environment, workers) = database::begin_environment_retirement(&environment.id)?;
        // Reviewers can nest under workers. Suspend/ForceDestroy acquire
        // parent then child, so retirement must take ancestors first too.
        // Keep every capability through Stop and removal; releasing one in
        // between would admit a checkpoint or a delivery into the target.
        let mut admitted = std::collections::BTreeSet::from([environment.owner_session_id.clone()]);
        for worker in &workers {
            let id = &worker.session_id;
            if admitted.insert(id.clone()) {
                let child = futures::executor::block_on(gate.enter_foreground(
                    id,
                    "environment retirement",
                    crate::recovery_gate::LifecycleOrigin::Automatic,
                    &std::sync::atomic::AtomicBool::new(false),
                    executor,
                ))?;
                permits.push(WorkerPermit::acquire(id, &child)?);
            }
        }
        let workers = workers
            .into_iter()
            .map(|worker| {
                let target = backend(&worker, config)
                    .with_context(|| format!("resolve environment worker {}", worker.session_id))?;
                Ok((worker, target))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut quiesced = Vec::new();
        for (worker, target) in &workers {
            if !targets::is_borrowed(target)
                && let Some(plan) = targets::stop_container_plan(target, &worker.session_id)?
            {
                plan.execute(executor)
                    .with_context(|| format!("stop environment worker {}", worker.session_id))?;
                quiesced.push(&worker.target);
            }
        }
        let results = std::thread::scope(|scope| {
            let handles = workers
                .iter()
                .filter(|(worker, _)| {
                    !quiesced
                        .iter()
                        .any(|stopped| stopped.same_resource(&worker.target))
                })
                .map(|(worker, target)| {
                    let permit = permits
                        .iter()
                        .find(|permit| permit.session_id() == worker.session_id)
                        .expect("environment worker permit disappeared");
                    scope.spawn(move || {
                        permit.scope_blocking(|| -> Result<()> {
                            let root = targets::worker_root(target, &worker.session_id)?;
                            // Retirement removes the root next, so the worker
                            // gets no TERM grace: nothing in it needs flushing.
                            if let Err(error) = super::worker_binary::kill_retired_worker(
                                permit, executor, target, &root,
                            ) {
                                if !targets::is_borrowed(target)
                                    && targets::cleanup_target_is_confirmed_absent(
                                        target,
                                        &worker.session_id,
                                        executor,
                                    )?
                                {
                                    return Ok(());
                                }
                                return Err(error);
                            }
                            Ok(())
                        })
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().unwrap_or_else(|panic| {
                        Err(anyhow::anyhow!(
                            "worker stop thread panicked: {}",
                            targets::command_thread_panic_message(panic.as_ref())
                        ))
                    })
                })
                .collect::<Vec<_>>()
        });
        let errors = workers
            .iter()
            .filter(|(worker, _)| {
                !quiesced
                    .iter()
                    .any(|stopped| stopped.same_resource(&worker.target))
            })
            .zip(results)
            .filter_map(|((worker, _), result)| {
                result.err().map(|error| {
                    format!(
                        "stop environment worker {} before removing shared files: {error:#}",
                        worker.session_id
                    )
                })
            })
            .collect::<Vec<_>>();
        ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(Self {
            environment,
            workers,
            _permits: permits,
        })
    }

    /// Remove private child roots before their enclosing target disappears.
    fn remove_targets(&self, executor: &impl CommandExecutor) -> Result<()> {
        if let Some(prepared) = &self.environment.placement.prepared_destination {
            super::move_session::destroy_prepared_environment(
                self,
                &self.environment.owner_session_id,
                &prepared.destination,
                executor,
            )?;
        }
        if let Some(target) = &self.environment.placement.target
            && matches!(
                target,
                mj_core::state::TargetLocator::LocalBare { .. }
                    | mj_core::state::TargetLocator::SshBare { .. }
            )
        {
            let backend = &self
                .workers
                .iter()
                .find(|(worker, _)| {
                    worker.session_id == self.environment.owner_session_id
                        && worker.target.same_resource(target)
                })
                .context("environment owner has no recorded worker placement")?
                .1;
            super::subagents::cleanup_owned_reports(
                self,
                backend,
                &self.environment.owner_session_id,
                &database::environment_reports(&self.environment.id)?,
                executor,
            )?;
        }
        for (worker, target) in &self.workers {
            if targets::is_borrowed(target) {
                // A removed container removes these private roots with it. A
                // bare child has a separate root that must be reclaimed here.
                if matches!(target, targets::TargetLocator::SshBare { .. }) {
                    targets::stopped_borrowed_worker_cleanup_plan(
                        self.stopped_worker(worker, target)?,
                    )?
                    .execute(executor)?;
                }
            } else if worker.session_id != self.environment.owner_session_id {
                execute_owned_target_cleanup(self, target, worker, executor)?;
            }
        }
        for (worker, target) in &self.workers {
            if worker.session_id == self.environment.owner_session_id {
                execute_owned_target_cleanup(self, target, worker, executor)?;
            }
        }
        // An environment may have a checkout before its first worker target.
        Ok(())
    }

    pub(super) fn record_prepared_instance(&self, instance_id: &str) -> Result<()> {
        database::record_prepared_environment_instance(&self.environment.id, instance_id)
    }

    pub fn finish(self) -> Result<()> {
        if self.environment.phase != EnvironmentPhase::Reclaimed {
            database::finish_environment_retirement(&self.environment.id)?;
        }
        Ok(())
    }
}

fn execute_owned_target_cleanup(
    authority: &Reclamation,
    target: &targets::TargetLocator,
    worker: &EnvironmentWorker,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let session_id = &worker.session_id;
    // Session caches can outlive one workspace generation during a Move.
    // Their owner decides from the durable inventory, not a caller's flag.
    let shared = database::worker_environments(&authority.environment.owner_session_id)?
        .into_iter()
        .any(|other| {
            other.id != authority.environment.id && other.phase != EnvironmentPhase::Reclaimed
        });
    let plan = if shared {
        targets::retire_stopped_move_target_plan(authority.stopped_worker(worker, target)?)?
    } else {
        targets::close_stopped_target_plan(authority.stopped_worker(worker, target)?)?
    };
    if let Err(error) = plan.execute(executor) {
        if !targets::cleanup_target_is_confirmed_absent(target, session_id, executor)? {
            return Err(error);
        }
        tracing::warn!(session_id,error=%error,"target cleanup failed but exact absence was confirmed");
    }
    if let Some(intent) = database::load_worker_restart(session_id)?
        && intent.target.same_resource(&worker.target)
    {
        database::finish_worker_restart(session_id, &intent.operation_id)?;
    }
    Ok(())
}

/// A child relinquishes execution, not the owner's files. Its lease remains
/// durable until the entire environment is reclaimed, including after Destroy.
pub(super) fn stop_borrower(
    session_id: &str,
    config: &Config,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let permit = crate::worker_lifecycle::require(session_id)?;
    let environments = database::worker_environments(session_id)?;
    for environment in environments
        .into_iter()
        .filter(|environment| environment.phase != EnvironmentPhase::Reclaimed)
    {
        for worker in database::environment_workers(&environment.id)?
            .into_iter()
            .filter(|worker| worker.session_id == session_id)
        {
            let target = backend(&worker, config)?;
            let root = targets::worker_root(&target, session_id)?;
            super::worker_binary::stop_worker(&permit, executor, &target, &root)?;
        }
    }
    Ok(())
}

pub(super) fn retire_session_environment(
    session: &SessionRecord,
    config: &Config,
    executor: &(impl CommandExecutor + Sync),
    branch: BranchDisposition,
) -> Result<()> {
    retire_session_environment_with_checkout(
        session,
        config,
        executor,
        branch,
        super::lifecycle::CheckoutDisposition::Remove,
    )
    .map(|_| ())
}

pub(super) fn retire_session_environment_with_checkout(
    session: &SessionRecord,
    config: &Config,
    executor: &(impl CommandExecutor + Sync),
    branch: BranchDisposition,
    checkout: super::lifecycle::CheckoutDisposition,
) -> Result<Option<std::path::PathBuf>> {
    let environment = database::session_environment(&session.id)?
        .context("session has no durable environment ownership")?;
    ensure!(
        environment.owner_session_id == session.id,
        "borrowed workspace must be retired by its owner"
    );
    if let Some(target) = &session.target {
        ensure!(
            environment
                .placement
                .target
                .as_ref()
                .is_some_and(|stored| stored.same_resource(target)),
            "session placement changed before environment cleanup"
        );
    }
    retire_owned_environment(environment, config, executor, branch, checkout)
}

pub(super) fn retire_owned_environment(
    environment: EnvironmentRecord,
    config: &Config,
    executor: &(impl CommandExecutor + Sync),
    branch: BranchDisposition,
    checkout: super::lifecycle::CheckoutDisposition,
) -> Result<Option<std::path::PathBuf>> {
    let session_id = environment.owner_session_id.clone();
    let mut retained = None;
    let authority = Reclamation::acquire(
        crate::worker_lifecycle::require(&environment.owner_session_id)
            .context("retire_owned_environment requires its caller's worker permit")?
            .admission(),
        environment,
        config,
        executor,
    )?;
    if authority.environment.phase == EnvironmentPhase::Reclaimed {
        if let Some(worktree) = &authority.environment.placement.managed_worktree {
            super::worktree::cleanup_managed_worktree_branch(executor, worktree, branch)?;
        }
        return Ok(None);
    }
    authority.remove_targets(executor)?;
    if let Some(worktree) = &authority.environment.placement.managed_worktree {
        let keep = checkout == super::lifecycle::CheckoutDisposition::KeepWhenDirty
            && (worktree.kind == mj_core::state::ManagedCheckoutKind::Clone
                || super::worktree::managed_worktree_checkout_is_dirty(executor, worktree)?);
        if keep {
            retained = Some(worktree.worktree_root.clone());
        } else {
            if branch == BranchDisposition::Keep {
                super::worktree::retire_managed_worktree(
                    &authority.checkout_removal()?,
                    executor,
                    worktree,
                )?;
            } else {
                super::worktree::cleanup_managed_worktree(
                    &authority.checkout_removal()?,
                    executor,
                    worktree,
                    branch,
                )?;
            }
        }
    }
    if let Some(target) = &authority.environment.placement.target {
        let placement = &authority.environment.placement;
        let target = recorded_backend_locator(
            target,
            &session_id,
            &placement.target_template_id,
            placement.runtime.as_ref(),
            config,
        )?;
        if let Some(release) = super::mbx::release::BuildStateRelease::for_environment(
            &session_id,
            placement,
            &target,
            config,
        ) {
            let live = database::read_durable_session_record(&session_id)?;
            let replacement = live.as_ref().filter(|live| {
                live.target.as_ref().is_some_and(|current| {
                    !current.same_resource(placement.target.as_ref().unwrap())
                })
            });
            let replacement_backend = replacement.and_then(|live| {
                match super::backend::backend_locator(live.target.as_ref().unwrap(),live,config) {
                    Ok(target) => Some((live,target)),
                    Err(error) => { tracing::warn!(session_id,error=%error,"replacement placement could not be resolved; retaining build state"); None }
                }
            });
            if (replacement.is_none() || replacement_backend.is_some())
                && let Some(release) = release.excluding(
                    replacement_backend
                        .as_ref()
                        .map(|(live, target)| (*live, target)),
                )
            {
                release.run(executor);
            }
        }
    }
    authority.finish()?;
    Ok(retained)
}

/// Suspend can move large Podman storage cleanup to its existing supervised
/// task. Admission is closed and managed Stops complete before it returns.
pub(super) fn quiesce_session_environment(
    session: &SessionRecord,
    config: &Config,
    executor: &(impl CommandExecutor + Sync),
) -> Result<()> {
    let environment =
        database::session_environment(&session.id)?.context("session has no environment")?;
    let authority = Reclamation::acquire(
        crate::worker_lifecycle::require(&environment.owner_session_id)
            .context("quiesce_session_environment requires its caller's worker permit")?
            .admission(),
        environment,
        config,
        executor,
    )?;
    if let Some(worktree) = &authority.environment.placement.managed_worktree {
        super::worktree::cleanup_managed_worktree(
            &authority.checkout_removal()?,
            executor,
            worktree,
            BranchDisposition::Keep,
        )?;
    }
    Ok(())
}

/// A deletion capability is scoped to one exact checkout and to the lifetime
/// of the retirement holding all worker launch permits, or to a just-created checkout
/// which has never been handed to a worker.
pub(super) struct CheckoutRemoval<'a> {
    checkout: &'a ManagedWorktree,
    _retirement: Option<&'a Reclamation>,
}
impl CheckoutRemoval<'_> {
    pub fn covers(&self, checkout: &ManagedWorktree) -> Result<()> {
        ensure!(
            self.checkout == checkout,
            "checkout deletion capability does not cover this checkout"
        );
        Ok(())
    }
}
impl Reclamation {
    fn checkout_removal(&self) -> Result<CheckoutRemoval<'_>> {
        ensure!(
            self.environment.phase == EnvironmentPhase::Retiring,
            "environment no longer owns checkout retirement"
        );
        Ok(CheckoutRemoval {
            checkout: self
                .environment
                .placement
                .managed_worktree
                .as_ref()
                .context("environment owns no checkout")?,
            _retirement: Some(self),
        })
    }
}

/// Roll back a checkout created during preparation, before any worker could
/// receive it. This explicit authority is only for that construction path.
#[cfg(any(test, feature = "test-support"))]
pub(super) fn cleanup_unstarted_checkout(
    executor: &impl CommandExecutor,
    checkout: &ManagedWorktree,
    branch: BranchDisposition,
) -> Result<()> {
    let authority = CheckoutRemoval {
        checkout,
        _retirement: None,
    };
    super::worktree::cleanup_managed_worktree(&authority, executor, checkout, branch)
}

/// Replace execution while retaining the owned workspace and its identity.
/// Failed removal leaves admission closed; successful removal retires the old
/// leases before reopening, under the same owner permit used by reclamation.
pub(super) fn remove_runtime_preserving_workspace(
    session: &SessionRecord,
    config: &Config,
    executor: &(impl CommandExecutor + Sync),
) -> Result<()> {
    let environment =
        database::session_environment(&session.id)?.context("session has no environment")?;
    if environment.phase == EnvironmentPhase::Reclaimed {
        return Ok(());
    }
    ensure!(
        environment.placement.target == session.target.clone().map(|v| *v),
        "runtime placement changed before replacement"
    );
    let authority = Reclamation::acquire(
        crate::worker_lifecycle::require(&environment.owner_session_id)
            .context("remove_runtime_preserving_workspace requires its caller's worker permit")?
            .admission(),
        environment,
        config,
        executor,
    )?;
    authority.remove_targets(executor)?;
    if authority.environment.placement.managed_worktree.is_none()
        && authority.environment.placement.project_directory.is_none()
    {
        // This target owned the workspace which removal just reclaimed. A
        // later restoration creates a fresh environment instead of reopening it.
        authority.finish()
    } else {
        database::finish_environment_runtime_removal(&authority.environment.id)
    }
}

/// Destroy relinquishes every environment the logical owner left behind,
/// including an interrupted Move. Failure of one never hides the others.
pub(super) fn destroy_session_environments(
    session: &SessionRecord,
    config: &Config,
    executor: &(impl CommandExecutor + Sync),
    branch: BranchDisposition,
) -> Result<()> {
    let environments = database::worker_environments(&session.id)?;
    let mut errors = Vec::new();
    for environment in environments
        .into_iter()
        .filter(|environment| environment.owner_session_id == session.id)
    {
        if let Err(error) = retire_owned_environment(
            environment,
            config,
            executor,
            branch,
            super::CheckoutDisposition::Remove,
        ) {
            errors.push(format!("{error:#}"));
        }
    }
    ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(())
}

/// Automatic permanent deletion must settle every old placement too; a failed
/// stop retains the logical record and all outstanding physical ownership.
pub(super) fn retire_inactive_environments(
    session: &SessionRecord,
    config: &Config,
    executor: &(impl CommandExecutor + Sync),
    branch: BranchDisposition,
    checkout: super::CheckoutDisposition,
) -> Result<Option<std::path::PathBuf>> {
    let mut retained = None;
    for environment in database::worker_environments(&session.id)?
        .into_iter()
        .filter(|environment| environment.owner_session_id == session.id)
    {
        if let Some(path) =
            retire_owned_environment(environment, config, executor, branch, checkout)?
        {
            retained = Some(path);
        }
    }
    Ok(retained)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::test_support::{IsolatedTest, checkpoint_test_session, test_name};
    use crate::targets::{CommandOutput, CommandSpec};
    use std::sync::{Arc, Mutex, mpsc};

    struct ControlledExecutor {
        waiting: mpsc::Sender<()>,
        operations: Mutex<Vec<String>>,
        stop_barrier: Option<std::sync::Barrier>,
        fail_stops: std::sync::atomic::AtomicBool,
    }
    impl CommandExecutor for ControlledExecutor {
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
            self.operations
                .lock()
                .unwrap()
                .push(command.purpose.clone());
            if command.purpose == "kill retired Mjolnir worker daemon" {
                if let Some(barrier) = &self.stop_barrier {
                    barrier.wait();
                }
                if self.fail_stops.load(std::sync::atomic::Ordering::SeqCst) {
                    anyhow::bail!("injected stop failure");
                }
            } else if command.purpose.contains("remove exact") {
                assert!(
                    !command
                        .args
                        .iter()
                        .any(|arg| arg.contains("worker stop --root")),
                    "reclamation must not repeat worker stopping"
                );
            }
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
        fn notify_notice(&self, notice: &str) {
            if notice.starts_with("Waiting for ") {
                self.waiting.send(()).unwrap();
            }
        }
    }

    // Hard-won: old-placement cleanup cleared the replacement worker's restart intent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    async fn retiring_a_move_source_preserves_the_destination_restart_intent() {
        const TEST: &str = "retiring_a_move_source_preserves_the_destination_restart_intent";
        const CHILD: &str = "MJ_TEST_MOVE_SOURCE_RETIREMENT";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(module_path!(), TEST))
                .isolated_store(directory.path())
                .env(CHILD, "1")
                .run();
            return;
        }
        let _writer = database::install_isolated_test_writer();
        let directory = tempfile::tempdir().unwrap();
        let mut session = checkpoint_test_session("11111111111111111111111111111111");
        session.target_template_id = "local".into();
        session.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: directory.path().join("source").join(&session.id),
        }));
        database::save_session(&session).unwrap();
        let source = database::session_environment(&session.id).unwrap().unwrap();
        session.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: directory.path().join("destination").join(&session.id),
        }));
        database::save_session(&session).unwrap();
        let (waiting, _) = mpsc::channel();
        let executor = ControlledExecutor {
            waiting,
            operations: Mutex::default(),
            stop_barrier: None,
            fail_stops: std::sync::atomic::AtomicBool::new(false),
        };
        let intent = database::WorkerRestartIntent {
            operation_id: "replacement".into(),
            target: *session.target.clone().unwrap(),
            desired_build: "test-build".into(),
        };
        let owner = crate::controller::test_support::worker_test_permit(
            &session.id,
            "Move destination launch",
        )
        .unwrap()
        .unwrap();
        owner
            .scope_blocking(|| {
                crate::controller::test_support::worker_test_run_blocking(
                    &session.id,
                    "Move destination launch",
                    &executor,
                    || {
                        database::begin_worker_restart(&session.id, &intent)?;
                        let authority = Reclamation::acquire(
                            crate::worker_lifecycle::require(&session.id)
                                .unwrap()
                                .admission(),
                            source,
                            &Config::default(),
                            &executor,
                        )
                        .unwrap();
                        authority.remove_targets(&executor).unwrap();
                        authority.finish()?;
                        Ok(())
                    },
                )
            })
            .unwrap();
        assert_eq!(
            database::load_worker_restart(&session.id).unwrap(),
            Some(intent)
        );
        database::admit_environment_worker(&session.id, session.target.as_ref().unwrap()).unwrap();
    }

    // Hard-won: #1506: retirement serialized historical workers and stopped each twice.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg_attr(
        windows,
        ignore = "#1291: local bare fixture; a Windows controller runs no local bare sessions"
    )]
    async fn retirement_drains_an_admitted_child_and_refuses_later_launch_and_registration() {
        const TEST: &str =
            "retirement_drains_an_admitted_child_and_refuses_later_launch_and_registration";
        const CHILD: &str = "MJ_TEST_ENVIRONMENT_ADMISSION";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            IsolatedTest::new(test_name(module_path!(), TEST))
                .isolated_store(directory.path())
                .env(CHILD, "1")
                .run();
            return;
        }
        let _writer = database::install_isolated_test_writer();
        let directory = tempfile::tempdir().unwrap();
        let mut parent = checkpoint_test_session("11111111111111111111111111111111");
        parent.state = mj_core::state::SessionState::Running;
        parent.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: directory.path().join(&parent.id),
        }));
        database::save_session(&parent).unwrap();
        let mut child = parent.clone();
        child.id = "22222222222222222222222222222222".into();
        child.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: directory.path().join(&child.id),
        }));
        let relation = mj_core::subagent::SubagentRecord {
            kind: mj_core::subagent::SubagentKind::Worker,
            reviewer_launch: None,
            child_session_id: child.id.clone(),
            parent_session_id: parent.id.clone(),
            task_name: "ownership race".into(),
            profile_id: "codex".into(),
            model: None,
            effort: None,
            working_directory: Default::default(),
            initial_prompt: "work".into(),
            request_key: "ownership-race".into(),
            created_at: parent.created_at.clone(),
            noticed_turn: None,
            reported_finish: None,
            handback_tool: false,
        };
        database::save_subagent_session(&child, &relation).unwrap();
        let mut grandchild = child.clone();
        grandchild.id = "01010101010101010101010101010101".into();
        grandchild.target = Some(Box::new(mj_core::state::TargetLocator::LocalBare {
            worker_root: directory.path().join(&grandchild.id),
        }));
        let mut grandchild_relation = relation.clone();
        grandchild_relation.kind = mj_core::subagent::SubagentKind::Reviewer;
        grandchild_relation.child_session_id = grandchild.id.clone();
        grandchild_relation.parent_session_id = child.id.clone();
        grandchild_relation.request_key = "grandchild-ownership-race".into();
        database::save_subagent_session(&grandchild, &grandchild_relation).unwrap();
        let (waiting_tx, waiting_rx) = mpsc::channel();
        let executor = Arc::new(ControlledExecutor {
            waiting: waiting_tx,
            operations: Mutex::default(),
            stop_barrier: Some(std::sync::Barrier::new(3)),
            fail_stops: std::sync::atomic::AtomicBool::new(true),
        });
        let gate = crate::controller::test_support::worker_test_gate();
        let child_admission = gate
            .enter_foreground(
                &child.id,
                "accepted child launch",
                crate::recovery_gate::LifecycleOrigin::Person,
                &std::sync::atomic::AtomicBool::new(false),
                executor.as_ref(),
            )
            .await
            .unwrap();
        let admitted = WorkerPermit::acquire(&child.id, &child_admission).unwrap();
        drop(child_admission);
        assert!(
            gate.try_start(
                &child.id,
                crate::recovery_gate::BackgroundWork::WorkerReplacement
            )
            .is_err()
        );
        database::admit_environment_worker(&child.id, child.target.as_ref().unwrap()).unwrap();
        let environment = database::session_environment(&parent.id).unwrap().unwrap();
        assert_eq!(
            database::begin_session_destroy(&parent.id).unwrap(),
            vec![child.id.clone()]
        );
        assert!(
            database::admit_environment_worker(&child.id, child.target.as_ref().unwrap()).is_err()
        );
        let mut later = child.clone();
        later.id = "33333333333333333333333333333333".into();
        let mut later_relation = relation;
        later_relation.child_session_id = later.id.clone();
        assert!(database::save_subagent_session(&later, &later_relation).is_err());
        let parent_admission = gate
            .enter_foreground(
                &parent.id,
                "environment retirement",
                crate::recovery_gate::LifecycleOrigin::Person,
                &std::sync::atomic::AtomicBool::new(false),
                executor.as_ref(),
            )
            .await
            .unwrap();
        let parent_admission = crate::recovery_gate::SessionAdmission::Foreground(parent_admission);
        let runtime = tokio::runtime::Handle::try_current().ok();
        let retiring = std::thread::spawn({
            let executor = executor.clone();
            let admission = parent_admission.clone();
            move || {
                let _entered = runtime.as_ref().map(tokio::runtime::Handle::enter);
                Reclamation::acquire(
                    &admission,
                    environment,
                    &Config::default(),
                    executor.as_ref(),
                )
            }
        });
        waiting_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(
            executor.operations.lock().unwrap().is_empty(),
            "no writer may be stopped or files removed before the accepted launch settles"
        );
        // A lower-ID grandchild must remain available to the child owner.
        // ID-only retirement ordering would hold it while waiting on child,
        // deadlocking the ordinary parent-to-descendant teardown order.
        let nested = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            gate.enter_foreground(
                &grandchild.id,
                "nested child teardown",
                crate::recovery_gate::LifecycleOrigin::Person,
                &std::sync::atomic::AtomicBool::new(false),
                executor.as_ref(),
            ),
        )
        .await
        .expect("retirement cannot take a descendant before its ancestor")
        .unwrap();
        drop(nested);
        drop(admitted);
        let error = match retiring.join().unwrap() {
            Ok(_) => panic!("stop failures must refuse reclamation"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(
            message.contains(&parent.id) && message.contains(&child.id),
            "{message}"
        );
        assert_eq!(
            executor.operations.lock().unwrap().len(),
            3,
            "no deletion after a stop failure"
        );
        executor.operations.lock().unwrap().clear();
        executor
            .fail_stops
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let environment = database::session_environment(&parent.id).unwrap().unwrap();
        let authority = Reclamation::acquire(
            &parent_admission,
            environment,
            &Config::default(),
            executor.as_ref(),
        )
        .unwrap();
        // The three workers stop concurrently: one kill per worker.
        let operations = executor.operations.lock().unwrap().clone();
        assert_eq!(
            operations,
            vec!["kill retired Mjolnir worker daemon"; 3],
            "{operations:?}"
        );
        authority.remove_targets(executor.as_ref()).unwrap();
        authority.finish().unwrap();
        assert!(
            database::admit_environment_worker(&child.id, child.target.as_ref().unwrap()).is_err()
        );
        assert!(
            !database::load_state()
                .unwrap()
                .sessions
                .contains_key(&later.id)
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetAvailability {
    Reachable,
    Missing,
    Unreachable,
}

pub fn target_availability(
    controller: &Controller,
    session_id: &str,
    executor: &impl CommandExecutor,
) -> Result<TargetAvailability> {
    target_availability_with_start(controller, session_id, executor, true)
}

fn target_availability_with_start(
    controller: &Controller,
    session_id: &str,
    executor: &impl CommandExecutor,
    start: bool,
) -> Result<TargetAvailability> {
    let record = controller
        .state
        .sessions
        .get(session_id)
        .with_context(|| format!("unknown session {session_id}"))?;
    let Some(locator) = record.target.as_ref() else {
        return Ok(TargetAvailability::Missing);
    };
    let locator = crate::controller::backend_locator(locator, record, &controller.config)?;

    if let Some(plan) = crate::targets::target_recovery_plan(&locator, session_id)? {
        let output = executor
            .execute(&plan.exists)
            .context("check retained target before Restart")?;
        match output.status {
            0 if !start => Ok(TargetAvailability::Reachable),
            0 => match crate::targets::ensure_recovery_target_running(executor, Some(&plan))? {
                crate::targets::TargetRecoveryOutcome::Missing => Ok(TargetAvailability::Missing),
                crate::targets::TargetRecoveryOutcome::AlreadyRunning
                | crate::targets::TargetRecoveryOutcome::Started
                | crate::targets::TargetRecoveryOutcome::NotRequired => {
                    Ok(TargetAvailability::Reachable)
                }
            },
            1 => Ok(TargetAvailability::Missing),
            125 => Ok(TargetAvailability::Unreachable),
            255 if locator_is_remote(&locator) => Ok(TargetAvailability::Unreachable),
            status => bail!(
                "could not confirm retained target availability before Restart (status {status}): {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    } else if let mj_core::targets::TargetLocator::AppleContainer { container_id, .. }
    | mj_core::targets::TargetLocator::SshAppleContainer { container_id, .. } = &locator
    {
        // Listing runs on the host that owns the runtime, which for a remote
        // target is the other side of ssh.
        let engine = locator.container_engine().expect("Apple container locator");
        let arguments = vec![
            engine.to_owned(),
            "list".to_owned(),
            "--all".to_owned(),
            "--quiet".to_owned(),
        ];
        let list = match &locator {
            mj_core::targets::TargetLocator::SshAppleContainer { ssh, .. } => {
                crate::targets::ssh_command_owned(SshTrafficClass::Control, ssh, arguments)
            }
            _ => {
                let mut arguments = arguments.into_iter();
                CommandSpec::new(
                    arguments.next().expect("engine"),
                    arguments.collect::<Vec<_>>(),
                )
            }
        }
        .purpose("check retained Apple container before Restart");
        let output = executor
            .execute(&list)
            .context("list retained Apple target before Restart")?;
        if output.status == 255 && locator_is_remote(&locator) {
            return Ok(TargetAvailability::Unreachable);
        }
        ensure!(
            output.status == 0,
            "could not confirm Apple target availability before Restart (status {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let listed = String::from_utf8(output.stdout)
            .context("decode Apple container list before Restart")?;
        if listed.lines().any(|id| id.trim() == container_id) {
            Ok(TargetAvailability::Reachable)
        } else {
            Ok(TargetAvailability::Missing)
        }
    } else {
        let probe = crate::targets::command_on_locator(
            SshTrafficClass::Control,
            &locator,
            session_id,
            vec!["true".into()],
            "probe retained target before Restart",
        )?;
        let output = executor
            .execute(&probe)
            .context("probe retained target before Restart")?;
        match output.status {
            0 => Ok(TargetAvailability::Reachable),
            255 if locator_is_remote(&locator) => Ok(TargetAvailability::Unreachable),
            status => bail!(
                "could not confirm retained target availability before Restart (status {status}); refusing destructive fallback: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        }
    }
}

fn locator_is_remote(locator: &mj_core::targets::TargetLocator) -> bool {
    matches!(
        locator,
        mj_core::targets::TargetLocator::AwsEc2 { .. }
            | mj_core::targets::TargetLocator::SshBare { .. }
            | mj_core::targets::TargetLocator::SshPodman { .. }
            | mj_core::targets::TargetLocator::SshDocker { .. }
            | mj_core::targets::TargetLocator::SshAppleContainer { .. }
            | mj_core::targets::TargetLocator::SshNitro { .. }
    )
}

/// Apple's default list contains only running containers (see cleanup.rs).
/// Resume may start the exact retained container; Restart's probe stays read-only.
fn start_retained_apple_for_resume(
    locator: &targets::TargetLocator,
    executor: &impl CommandExecutor,
) -> Result<()> {
    let (ssh, container_id) = match locator {
        targets::TargetLocator::AppleContainer { container_id, .. } => (None, container_id),
        targets::TargetLocator::SshAppleContainer {
            ssh, container_id, ..
        } => (Some(ssh), container_id),
        _ => return Ok(()),
    };
    let command = |args: Vec<String>, purpose| {
        match ssh {
            Some(ssh) => targets::ssh_command_owned(SshTrafficClass::Control, ssh, args),
            None => CommandSpec::new(&args[0], args[1..].to_vec()),
        }
        .purpose(purpose)
    };
    let running = command(
        vec!["container".into(), "list".into(), "--quiet".into()],
        "list running retained Apple containers for Resume",
    );
    let is_running = || -> Result<bool> {
        let output = executor.execute(&running)?;
        ensure!(
            output.status == 0,
            "list running Apple containers: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?
            .lines()
            .any(|id| id.trim() == container_id))
    };
    if !is_running()? {
        let start = command(
            vec!["container".into(), "start".into(), container_id.clone()],
            "start retained Apple container for Resume",
        );
        let output = executor.execute(&start)?;
        ensure!(
            output.status == 0,
            "start retained Apple container: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        ensure!(
            is_running()?,
            "retained Apple container did not become running"
        );
    }
    Ok(())
}

/// Directory evidence shared by advisory preflight and the lifecycle decision.
pub(super) fn directory_available(
    backend: &targets::TargetLocator,
    directory: &str,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    let output = executor.execute(&super::worker_binary::directory_presence_command(
        backend, directory,
    ))?;
    ensure!(
        output.status == 0,
        "cannot inspect retained directory {directory:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    match String::from_utf8_lossy(&output.stdout).trim() {
        "present" => Ok(true),
        "absent" => Ok(false),
        value => bail!("unexpected directory presence response {value:?}"),
    }
}

fn retained_container_checkout_available(
    controller: &Controller,
    session_id: &str,
    backend: &targets::TargetLocator,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    if targets::retained_workspace_volume_exists(backend, session_id, executor)? {
        return Ok(true);
    }
    let record = &controller.state.sessions[session_id];
    let workspace = PathBuf::from(super::network_git::workspace_root(
        backend,
        record.container_workspace.as_deref(),
    ));
    let checkouts = resume_checkout_directories(controller, session_id, backend)?;
    let mut paths = std::collections::BTreeSet::new();
    match backend {
        targets::TargetLocator::LocalPodman {
            workspace_storage: targets::PodmanWorkspaceLocator::HostPath { path, .. },
            ..
        }
        | targets::TargetLocator::SshPodman {
            workspace_storage: targets::PodmanWorkspaceLocator::HostPath { path, .. },
            ..
        } => {
            paths.insert(PathBuf::from(path));
        }
        _ => {}
    }
    for mount in mj_core::state::applied_container_settings(record).additional_mounts {
        for directory in checkouts.iter().chain(std::iter::once(&workspace)) {
            if let Ok(relative) = directory.strip_prefix(&mount.destination) {
                paths.insert(mount.source.join(relative));
            }
        }
        if mount.destination.starts_with(&workspace) {
            paths.insert(mount.source.clone());
        }
    }
    let host = match backend {
        targets::TargetLocator::SshPodman { ssh, .. }
        | targets::TargetLocator::SshDocker { ssh, .. }
        | targets::TargetLocator::SshAppleContainer { ssh, .. }
        | targets::TargetLocator::SshNitro { ssh, .. } => targets::TargetLocator::SshBare {
            ssh: ssh.clone(),
            workspace: String::new(),
            worker_id: None,
        },
        _ => targets::TargetLocator::LocalBare {
            worker_root: String::new(),
        },
    };
    let mut present = false;
    let mut unknown = None;
    for path in paths {
        match directory_available(&host, &path.to_string_lossy(), executor) {
            Ok(found) => present |= found,
            Err(error) => unknown = Some(error),
        }
    }
    if present {
        Ok(true)
    } else if let Some(error) = unknown {
        Err(error)
    } else {
        Ok(false)
    }
}

pub(super) fn resume_checkout_available(
    controller: &Controller,
    session_id: &str,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    let (backend, _) = controller.worker_placement(session_id)?;
    match target_availability_with_start(controller, session_id, executor, false)? {
        TargetAvailability::Unreachable => bail!("target engine is unreachable"),
        TargetAvailability::Missing => {
            return retained_container_checkout_available(
                controller, session_id, &backend, executor,
            );
        }
        TargetAvailability::Reachable => {}
    }
    let (_, root) = controller.worker_placement(session_id)?;
    let pending = executor.execute(&super::worker_binary::worker_recovery_command(
        &backend, &root,
    ))?;
    ensure!(
        pending.status == 0,
        "cannot inspect pending checkout recovery"
    );
    if String::from_utf8_lossy(&pending.stdout).trim() == "checkout-partial" {
        return Ok(false);
    }
    resume_checkouts_present(controller, session_id, &backend, executor)
}

pub(super) fn resume_checkout_directories(
    controller: &Controller,
    session_id: &str,
    backend: &targets::TargetLocator,
) -> Result<Vec<PathBuf>> {
    if matches!(
        controller.state.checkout(session_id)?.effective(),
        mj_core::state::Checkout::ManagedWorkspace
    ) {
        let record = &controller.state.sessions[session_id];
        if let Some(bundle) = record.project_bundle(&controller.config) {
            let workspace = PathBuf::from(super::network_git::workspace_root(
                backend,
                record.container_workspace.as_deref(),
            ));
            return Ok(bundle
                .repositories
                .iter()
                .map(|repo| workspace.join(&repo.destination))
                .collect());
        }
    }
    Ok(vec![PathBuf::from(resume_checkout_directory(
        controller, session_id, backend,
    )?)])
}

fn resume_checkouts_present(
    controller: &Controller,
    session_id: &str,
    backend: &targets::TargetLocator,
    executor: &impl CommandExecutor,
) -> Result<bool> {
    let mut present = true;
    for directory in resume_checkout_directories(controller, session_id, backend)? {
        present &= directory_available(backend, &directory.to_string_lossy(), executor)?;
    }
    Ok(present)
}

pub(super) fn resume_checkout_directory(
    controller: &Controller,
    session_id: &str,
    backend: &targets::TargetLocator,
) -> Result<String> {
    let session = &controller.state.sessions[session_id];
    if let Some(path) = controller.state.checkout(session_id)?.project_directory() {
        return Ok(path.to_string_lossy().into_owned());
    }
    let root = PathBuf::from(super::network_git::workspace_root(
        backend,
        session.container_workspace.as_deref(),
    ));
    Ok(session
        .project_bundle(&controller.config)
        .and_then(|bundle| bundle.primary())
        .map(|primary| root.join(&primary.destination))
        .unwrap_or(root)
        .to_string_lossy()
        .into_owned())
}

/// Evidence for Resume, obtained while its worker lifecycle permit is held.
pub(super) enum ResumeEnvironment {
    LiveWorker,
    WorkerMissing,
    ParentDataMissing,
    RetainedContainerMissing,
    CheckoutMissing,
    Absent,
    Unknown(anyhow::Error),
}

pub(super) fn classify_resume_environment(
    controller: &Controller,
    session_id: &str,
    executor: &impl CommandExecutor,
) -> ResumeEnvironment {
    let probe = || -> Result<ResumeEnvironment> {
        let (backend, root) = controller.worker_placement(session_id)?;
        match target_availability(controller, session_id, executor)? {
            TargetAvailability::Missing => {
                return Ok(
                    if retained_container_checkout_available(
                        controller, session_id, &backend, executor,
                    )? {
                        ResumeEnvironment::RetainedContainerMissing
                    } else {
                        ResumeEnvironment::Absent
                    },
                );
            }
            TargetAvailability::Unreachable => bail!("the target engine is unreachable"),
            TargetAvailability::Reachable => {}
        }
        start_retained_apple_for_resume(&backend, executor)?;
        let output =
            super::worker_binary::probe_worker_liveness(executor, &backend, session_id, &root)?;
        ensure!(
            output.status == 0,
            "worker liveness probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let live = match String::from_utf8_lossy(&output.stdout).trim() {
            "alive" | "starting" => true,
            "dead" => false,
            value => bail!("unexpected worker liveness response {value:?}"),
        };
        let checkout = resume_checkouts_present(controller, session_id, &backend, executor)?;
        let output = executor.execute(&super::worker_binary::worker_recovery_command(
            &backend, &root,
        ))?;
        ensure!(
            output.status == 0,
            "cannot inspect retained parent runtime: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let recovery = String::from_utf8_lossy(&output.stdout);
        if live && recovery.trim() != "checkout-partial" {
            return Ok(if checkout {
                ResumeEnvironment::LiveWorker
            } else {
                ResumeEnvironment::CheckoutMissing
            });
        }
        let runtime = match recovery.trim() {
            "partial" => {
                return Ok(if checkout {
                    ResumeEnvironment::ParentDataMissing
                } else {
                    ResumeEnvironment::CheckoutMissing
                });
            }
            "checkout-partial" => return Ok(ResumeEnvironment::CheckoutMissing),
            "recoverable" => true,
            "nothing" | "absent" => false,
            value => bail!("unexpected parent runtime response {value:?}"),
        };
        let profile = controller
            .config
            .named_profile(&controller.state.sessions[session_id].last_profile)?;
        let native = directory_available(
            &backend,
            &super::target_profile_home(&backend, session_id, profile),
            executor,
        )?;
        Ok(
            if !checkout && !runtime && !native && backend.container_engine().is_none() {
                ResumeEnvironment::Absent
            } else if !checkout {
                ResumeEnvironment::CheckoutMissing
            } else if (!runtime || !native)
                && controller.state.sessions[session_id]
                    .native_session_id
                    .is_some()
            {
                ResumeEnvironment::ParentDataMissing
            } else {
                ResumeEnvironment::WorkerMissing
            },
        )
    };
    probe().unwrap_or_else(ResumeEnvironment::Unknown)
}

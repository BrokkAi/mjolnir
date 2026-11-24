//! `Controller`'s methods for launching a worker, which read its configuration
//! and records through `crate::worker_launch`.

use super::worker_binary::launch::*;
use super::*;
use crate::worker_launch::{self, StagedWorkerProfile};
#[cfg(test)]
use mj_core::worker_launch::ProjectMemoryLaunchConfig;
use mj_core::worker_launch::WorkerLaunchConfig;
use mj_session_types::{ProjectMemorySyncTarget, WorkerRecoveryPlan};

impl Controller {
    #[cfg(test)]
    pub(crate) fn aws_credential_source(
        &self,
        session_id: &str,
    ) -> Result<Option<mj_core::aws_credentials::AwsCredentialSource>> {
        worker_launch::aws_credential_source(&self.config, &self.state, session_id)
    }

    /// A notice for the person when the profile's `[profiles.<id>.aws]` role
    /// cannot apply to this session, and why: its target is raw local or EC2,
    /// or sets its own AWS credentials, or the profile is not a Bedrock one.
    #[cfg(test)]
    pub(crate) fn aws_role_notice(&self, session_id: &str) -> Result<Option<String>> {
        worker_launch::aws_role_notice(&self.config, &self.state, session_id)
    }

    /// Where this session's worker lives. This is decided from the session
    /// record and configuration alone, so a caller can name the worker root
    /// before anything is installed into it.
    pub(in crate::controller) fn worker_placement(
        &self,
        session_id: &str,
    ) -> Result<(targets::TargetLocator, String)> {
        worker_launch::worker_placement(&self.config, &self.state, session_id)
    }

    pub(in crate::controller) fn prepare_worker_files(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
        worker_root: &str,
        executor: &impl CommandExecutor,
        history: LaunchHistory<'_>,
    ) -> Result<Option<String>> {
        worker_launch::prepare_worker_files(
            &self.config,
            &self.state,
            session_id,
            backend,
            worker_root,
            executor,
            history,
        )
    }

    pub(in crate::controller) fn prepare_worker_files_for(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
        worker_root: &str,
        executor: &impl CommandExecutor,
        preparation: WorkerFilePreparation,
        history: LaunchHistory<'_>,
    ) -> Result<StagedWorkerProfile> {
        worker_launch::prepare_worker_files_for(
            &self.config,
            &self.state,
            session_id,
            backend,
            worker_root,
            executor,
            preparation,
            history,
        )
    }

    /// Return the pending harness preflight so a new checkout can finish first,
    /// and the staged credential identities.
    pub(in crate::controller) fn stage_worker_files_for(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
        worker_root: &str,
        executor: &impl CommandExecutor,
        preparation: WorkerFilePreparation,
        history: LaunchHistory<'_>,
    ) -> Result<(Option<WorkerLaunchConfig>, StagedWorkerProfile)> {
        worker_launch::stage_worker_files_for(
            &self.config,
            &self.state,
            session_id,
            backend,
            worker_root,
            executor,
            preparation,
            history,
        )
    }

    pub(in crate::controller) fn prepare_build_cache_links(
        &self,
        session: &mj_core::state::SessionRecord,
        backend: &targets::TargetLocator,
        launch: &WorkerLaunchConfig,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        worker_launch::prepare_build_cache_links(session, backend, launch, executor)
    }

    /// Replace the installed launch configuration of a session whose worker is
    /// not running. A session with a build cache first gets the private mbx and
    /// policy snapshot that configuration names, so no restart can boot with a
    /// `MJ_MBX_CONFIG_DIR` whose files are not there.
    pub(in crate::controller) fn install_launch_config(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
        launch: &WorkerLaunchConfig,
        executor: &impl CommandExecutor,
    ) -> Result<()> {
        if let Some(session) = self.state.sessions.get(session_id) {
            self.prepare_build_cache_links(session, backend, launch, executor)
                .context("stage the session's mbx before installing its launch configuration")?;
        }
        super::worker_binary::replace_installed_worker_launch_config(
            executor, backend, session_id, launch,
        )
        .context("install the current Mjolnir worker launch configuration")
    }

    /// Probe the installed binary and the worker's recorded state after a
    /// session becomes unreachable. Returns `None` only when the session has
    /// no target to probe; a probe that fails says why.
    pub fn diagnose_worker(&self, session_id: &str) -> Option<String> {
        worker_launch::diagnose_worker(&self.config, &self.state, session_id)
    }

    pub fn diagnose_worker_controlled(
        &self,
        session_id: &str,
        executor: &impl CommandExecutor,
    ) -> Option<String> {
        worker_launch::diagnose_worker_controlled(&self.config, &self.state, session_id, executor)
    }

    /// A non-destructive liveness probe plus commands that replace a confirmed
    /// dead session worker without touching its durable relay files. The
    /// session manager runs both off its async actor.
    pub fn worker_recovery_plan(
        &self,
        session_id: &str,
        operation: Option<&mj_core::state::MoveOperation>,
    ) -> Result<WorkerRecoveryPlan> {
        worker_launch::worker_recovery_plan(&self.config, &self.state, session_id, operation)
    }

    /// The launch config for a session, including what depends on its
    /// sub-agent role. The first launch and every relaunch use this, so a
    /// relaunched worker keeps its delegation tools and a relaunched child
    /// keeps its parent's workspace.
    #[cfg(test)]
    pub(in crate::controller) fn session_launch_config(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
        history: LaunchHistory<'_>,
    ) -> Result<(
        WorkerLaunchConfig,
        Option<ProjectMemoryLaunchConfig>,
        String,
    )> {
        worker_launch::session_launch_config(
            &self.config,
            &self.state,
            session_id,
            backend,
            history,
        )
    }

    pub(in crate::controller) fn current_worker_launch_config(
        &self,
        session_id: &str,
        backend: &targets::TargetLocator,
    ) -> Result<WorkerLaunchConfig> {
        worker_launch::current_worker_launch_config(&self.config, &self.state, session_id, backend)
    }

    /// Where the session's project memory is synchronized, or `None` for a
    /// session that has none.
    pub fn project_memory_sync_target(
        &self,
        session_id: &str,
    ) -> Result<Option<ProjectMemorySyncTarget>> {
        worker_launch::project_memory_sync_target(&self.config, &self.state, session_id)
    }
}

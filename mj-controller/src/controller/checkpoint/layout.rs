use super::*;

impl Controller {
    /// Report roots may be shared by several parents; the persisted child
    /// directory is the ownership boundary, never a scan of that shared root.
    pub(super) fn checkpoint_report_directories(&self, session_id: &str) -> Result<Vec<PathBuf>> {
        let mut directories = Vec::new();
        for child in self
            .state
            .subagents
            .values()
            .filter(|child| child.parent_session_id == session_id)
        {
            if let Some(directory) =
                crate::database::load_subagent_report(&child.child_session_id)?.report_dir
            {
                let path = PathBuf::from(directory);
                ensure!(
                    path.file_name().and_then(|name| name.to_str())
                        == Some(child.child_session_id.as_str()),
                    "persisted report directory does not name its child"
                );
                directories.push(path);
            }
        }
        directories.sort();
        Ok(directories)
    }

    /// Where a session's repositories live on its target, and what each one
    /// contributes to an export.
    ///
    /// A checkpoint and a diff, a file read or a branch push all need the same
    /// answers - which target, which directory, which repository is the
    /// primary - so they are derived once here rather than restated wherever a
    /// caller reaches the target.
    pub fn session_export_layout(
        &self,
        session_id: &str,
        executor: &(impl CommandExecutor + Sync),
    ) -> Result<SessionExportLayout> {
        let session = self
            .state
            .sessions
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?
            .clone();
        let checkout = self.state.checkout(session_id)?;
        let locator = session
            .target
            .as_ref()
            .context("session has no live target")?;
        let backend = backend_locator(locator, &session, &self.config)?;
        let (workspace_root, primary_repository, repositories) =
            if let Some(project_directory) = checkout.project_directory() {
                let parent = project_directory
                    .parent()
                    .context("bare project directory has no parent")?;
                let destination = project_directory
                    .file_name()
                    .context("bare project directory cannot be the filesystem root")?;
                (
                    parent.to_string_lossy().into_owned(),
                    HOST_CHECKOUT_REPOSITORY.to_owned(),
                    vec![CheckpointRepositorySpec {
                        id: HOST_CHECKOUT_REPOSITORY.into(),
                        relative_destination: PathBuf::from(destination),
                        // Managed worktrees are retired on Stop, so their
                        // dirty/untracked state must travel in the archive.
                        // Capturing from the branch's creation point means the
                        // bundle also carries the session's own commits. The
                        // branch and objects remain in the owning Git
                        // repository as well; no remote origin is required.
                        // Unmanaged raw checkouts remain in place.
                        capture: match &checkout {
                            mj_core::state::Checkout::ManagedWorktree { worktree, .. } => {
                                let base_commit =
                                    crate::controller::worktree::managed_worktree_base_commit(
                                        worktree, executor,
                                    )?;
                                if worktree.kind == mj_core::state::ManagedCheckoutKind::Clone {
                                    CheckpointRepositoryCapture::ManagedClone { base_commit }
                                } else {
                                    CheckpointRepositoryCapture::DeltaFrom { base_commit }
                                }
                            }
                            _ => CheckpointRepositoryCapture::MetadataOnly,
                        },
                        origin_override: None,
                    }],
                )
            } else {
                let bundle = session
                    .project_bundle(&self.config)
                    .context("session bundle is missing")?;
                let workspace_root = crate::controller::network_git::workspace_root(
                    &backend,
                    session.container_workspace.as_deref(),
                );
                // A checkout without later history has no origin remote, so
                // the archive records the one the session was created from.
                let cutoff_origin = |repository: &mj_core::config::ProjectRepository| {
                    session
                        .checkout
                        .as_ref()
                        .filter(|checkout| {
                            checkout.no_later_history && checkout.repository_id == repository.id
                        })
                        .and(session.project.as_ref())
                        .and_then(|project| project.network_sources.get(&repository.id))
                        .map(|source| source.fetch_url.clone())
                };
                let repositories = bundle
                    .repositories
                    .iter()
                    .map(|repository| CheckpointRepositorySpec {
                        id: repository.id.clone(),
                        relative_destination: repository.destination.clone(),
                        capture: CheckpointRepositoryCapture::RemoteWorkspace,
                        origin_override: cutoff_origin(repository),
                    })
                    .collect();
                (workspace_root, bundle.primary_repo.clone(), repositories)
            };
        let (report_root, _) =
            crate::controller::subagents::subagent_report_root(&session, &backend, &checkout);
        Ok(SessionExportLayout {
            report_root,
            backend,
            workspace_root,
            primary_repository,
            repositories,
            managed_worktree: match checkout {
                mj_core::state::Checkout::ManagedWorktree { worktree, .. } => {
                    Some(worktree.clone())
                }
                _ => None,
            },
        })
    }
}

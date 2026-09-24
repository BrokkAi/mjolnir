use super::*;

impl Controller {
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
        let locator = session
            .target
            .as_ref()
            .context("session has no live target")?;
        let backend = backend_locator(locator, &session, &self.config)?;
        let (workspace_root, primary_repository, repositories) =
            if let Some(project_directory) = &session.project_directory {
                let parent = project_directory
                    .parent()
                    .context("bare project directory has no parent")?;
                let destination = project_directory
                    .file_name()
                    .context("bare project directory cannot be the filesystem root")?;
                (
                    parent.to_string_lossy().into_owned(),
                    "project".to_owned(),
                    vec![CheckpointRepositorySpec {
                        id: "project".into(),
                        relative_destination: PathBuf::from(destination),
                        // Managed worktrees are retired on Stop, so their
                        // dirty/untracked state must travel in the archive.
                        // Capturing from the branch's creation point means the
                        // bundle also carries the session's own commits. The
                        // branch and objects remain in the owning Git
                        // repository as well; no remote origin is required.
                        // Unmanaged raw checkouts remain in place.
                        capture: match &session.managed_worktree {
                            Some(worktree) => {
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
                            None => CheckpointRepositoryCapture::MetadataOnly,
                        },
                        origin_override: None,
                    }],
                )
            } else {
                let bundle = self
                    .config
                    .bundles
                    .get(&session.bundle_id)
                    .context("session bundle is missing")?;
                let workspace_root = crate::controller::network_git::workspace_root(
                    &backend,
                    session.container_workspace.as_deref(),
                );
                let repositories = bundle
                    .repositories
                    .iter()
                    .map(|repository| CheckpointRepositorySpec {
                        id: repository.id.clone(),
                        relative_destination: repository.destination.clone(),
                        capture: CheckpointRepositoryCapture::RemoteWorkspace,
                        origin_override: None,
                    })
                    .collect();
                (workspace_root, bundle.primary_repo.clone(), repositories)
            };
        Ok(SessionExportLayout {
            backend,
            workspace_root,
            primary_repository,
            repositories,
            managed_worktree: session.managed_worktree,
        })
    }
}

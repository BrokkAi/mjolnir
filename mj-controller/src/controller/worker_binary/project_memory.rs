use super::*;

pub(super) fn project_memory_launch(
    session: &mj_core::state::SessionRecord,
    bundle: Option<&ProjectBundle>,
    workspace: &(String, Vec<String>),
    target_profile_home: &str,
) -> Result<ProjectMemoryLaunchConfig> {
    // A sub-agent records its parent's checkout as its project directory.
    // For an isolated parent that is the parent's own clone, not the project,
    // so the child takes the parent's checkout for its identity and shares
    // the parent's memory (R10-4).
    let parent_worktree = match &session.managed_worktree {
        Some(_) => None,
        None => crate::database::load_subagent_parent_worktree(&session.id)?,
    };
    let identity = if let Some(worktree) = session
        .managed_worktree
        .as_ref()
        .or(parent_worktree.as_ref())
    {
        ProjectMemoryIdentity::Repository {
            repository: RepositoryMemoryIdentity::Local {
                canonical_root: std::fs::canonicalize(&worktree.source_repository)
                    .unwrap_or_else(|_| worktree.source_repository.clone()),
            },
        }
    } else if let Some(bundle) = bundle {
        let primary =
            configured_memory_identity(bundle.primary().context("bundle primary is missing")?)?;
        let members = bundle
            .repositories
            .iter()
            .map(configured_memory_identity)
            .collect::<Result<Vec<_>>>()?;
        ProjectMemoryIdentity::bundle(primary, members)
    } else {
        let project = session
            .project_directory
            .as_ref()
            .context("raw session project directory is missing")?;
        let repository = match session.target.as_ref() {
            Some(mj_core::state::TargetLocator::LocalBare { .. }) => {
                RepositoryMemoryIdentity::Local {
                    canonical_root: std::fs::canonicalize(project)
                        .unwrap_or_else(|_| project.clone()),
                }
            }
            _ => RepositoryMemoryIdentity::Remote {
                target: session.target_template_id.clone(),
                canonical_root: project.clone(),
            },
        };
        ProjectMemoryIdentity::Repository { repository }
    };
    let project_key = identity.key()?;
    let replica_slug = project_memory_replica_slug(&project_key, &session.id);
    let project_root = PathBuf::from(target_profile_home)
        .join("projects")
        .join(replica_slug);
    let root = project_root.join("memory");
    let baseline_root = project_root.join(".hel-memory-baseline");
    let mut repository_roots = std::collections::BTreeMap::new();
    if let Some(bundle) = bundle {
        let target_roots =
            std::iter::once(workspace.0.as_str()).chain(workspace.1.iter().map(String::as_str));
        let repositories = std::iter::once(bundle.primary().context("bundle primary is missing")?)
            .chain(
                bundle
                    .repositories
                    .iter()
                    .filter(|repository| repository.id != bundle.primary_repo),
            );
        repository_roots.extend(
            repositories
                .zip(target_roots)
                .map(|(repository, root)| (repository.id.clone(), PathBuf::from(root))),
        );
    }
    Ok(ProjectMemoryLaunchConfig {
        history_socket: None,
        project_key,
        root,
        baseline_root,
        repository_roots,
        mcp_delivery: ProjectMemoryMcpDelivery::Acp,
    })
}

pub(super) fn project_memory_replica_slug(project_key: &str, session_id: &str) -> String {
    format!("hel-{}-{session_id}", &project_key[..16])
}

pub(super) fn project_memory_mcp_delivery(
    harness: mj_core::config::HarnessKind,
    target: &targets::TargetLocator,
) -> ProjectMemoryMcpDelivery {
    if harness == mj_core::config::HarnessKind::Kimi
        && !matches!(target, targets::TargetLocator::LocalBare { .. })
    {
        ProjectMemoryMcpDelivery::HarnessProfile
    } else {
        ProjectMemoryMcpDelivery::Acp
    }
}

pub(super) fn configured_memory_identity(
    repository: &ProjectRepository,
) -> Result<RepositoryMemoryIdentity> {
    if let Some(source) = repository.github.as_deref() {
        let github = crate::setup::github_repository_from_origin(source)
            .with_context(|| format!("parse repository source {source:?} for project memory"))?;
        return Ok(RepositoryMemoryIdentity::Github {
            owner: github.owner.to_ascii_lowercase(),
            repository: github.repository.to_ascii_lowercase(),
        });
    }
    let root = repository
        .local
        .as_ref()
        .context("project repository has no source for memory identity")?;
    Ok(RepositoryMemoryIdentity::Local {
        canonical_root: mj_core::local_git::main_worktree_root(root)
            .or_else(|_| std::fs::canonicalize(root).map_err(anyhow::Error::from))
            .unwrap_or_else(|_| root.clone()),
    })
}

pub(super) fn canonical_memory_root(project_key: &str) -> PathBuf {
    data_dir().join("projects").join(project_key).join("memory")
}

pub(super) fn stage_memory_replica(
    memory: &ProjectMemoryLaunchConfig,
    target_profile_home: &Path,
    profile_stage: &Path,
) -> Result<()> {
    let canonical = canonical_memory_root(&memory.project_key);
    std::fs::create_dir_all(&canonical)?;
    let replica = memory.root.strip_prefix(target_profile_home)?;
    let baseline = memory.baseline_root.strip_prefix(target_profile_home)?;
    copy_profile_entry(&canonical, &profile_stage.join(replica))?;
    copy_profile_entry(&canonical, &profile_stage.join(baseline))
}

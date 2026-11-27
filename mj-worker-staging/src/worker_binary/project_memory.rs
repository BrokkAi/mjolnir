use super::*;

/// Claude Code's switch for its auto-memory directory and memory reminder.
pub const CLAUDE_DISABLE_AUTO_MEMORY_ENV: &str = "CLAUDE_CODE_DISABLE_AUTO_MEMORY";

pub fn project_memory_launch(
    session: &mj_core::state::SessionRecord,
    checkout: &mj_core::state::Checkout<'_>,
    bundle: Option<&ProjectBundle>,
    workspace: &(String, Vec<String>),
    target_profile_home: &str,
    parent_worktree: Option<&mj_core::state::ManagedWorktree>,
    pinned_key: Option<&str>,
) -> Result<ProjectMemoryLaunchConfig> {
    // A sub-agent works in its owner's checkout. For an isolated owner that is
    // the owner's own clone, not the project, so the child takes the owner's
    // checkout for its identity and shares the owner's memory (R10-4).
    let project_key = if let Some(project) = &session.project {
        project.memory_identity()?.key()?
    } else if let Some(key) = pinned_key {
        key.to_owned()
    } else {
        ProjectMemoryIdentity::for_legacy_session(session, checkout, bundle, parent_worktree)?
            .key()?
    };
    let replica_slug = project_memory_replica_slug(&project_key, &session.id);
    let project_root = mj_core::target_path::join(
        Path::new(target_profile_home),
        &Path::new("projects").join(replica_slug),
    );
    let root = mj_core::target_path::join(&project_root, Path::new("memory"));
    let baseline_root =
        mj_core::target_path::join(&project_root, Path::new(".hel-memory-baseline"));
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

pub fn project_memory_replica_slug(project_key: &str, session_id: &str) -> String {
    format!("hel-{}-{session_id}", &project_key[..16])
}

pub fn project_memory_mcp_delivery(
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

pub fn canonical_memory_root(project_key: &str) -> PathBuf {
    DataLayout::current()
        .projects_dir()
        .join(project_key)
        .join("memory")
}

pub fn stage_memory_seed(memory: &ProjectMemoryLaunchConfig, seed: &Path) -> Result<()> {
    let snapshot =
        mj_core::project_memory::read_canonical(&canonical_memory_root(&memory.project_key))?;
    // An empty snapshot still needs a directory to transfer to the worker.
    std::fs::create_dir_all(seed)?;
    mj_core::project_memory::render_memory_tree(seed, &snapshot, false)?;
    Ok(())
}

/// Read the canonical-memory snapshot already copied into this launch's stage.
pub fn staged_memory_index(seed: &Path) -> Result<Option<String>> {
    mj_core::project_memory::ProjectMemoryStore::new(seed).startup_index()
}

/// Transfer the seed independently of the profile overlay.
pub fn install_memory_seed(
    executor: &impl CommandExecutor,
    locator: &targets::TargetLocator,
    session_id: &str,
    seed: &Path,
    destination: &Path,
) -> Result<()> {
    let mut archive = tar::Builder::new(Vec::new());
    archive.append_dir_all(".", seed)?;
    let bytes = archive.into_inner()?;
    let command = targets::command_on_locator(
        mj_core::targets::SshTrafficClass::Bulk,
        locator,
        session_id,
        vec![
            "sh".into(),
            "-c".into(),
            "umask 077; rm -rf -- \"$1\"; mkdir -p -- \"$1\"; exec tar -xf - -C \"$1\"".into(),
            "mj-memory-seed".into(),
            destination.to_string_lossy().into_owned(),
        ],
        "install project memory seed",
    )?;
    let output = executor.execute_with_stdin(&command, &mut bytes.as_slice())?;
    targets::checked_command_output(&command, output)?;
    Ok(())
}

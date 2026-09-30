use super::*;

pub(crate) use mj_core::repository::RepositoryIdentity;

pub(super) fn root_identity(root: &Path) -> Result<RepositoryIdentity> {
    mj_core::repository::local_identity(
        root,
        &mj_core::targets::CancellableProcessExecutor::with_timeout(Duration::from_secs(8)),
    )
}

pub(crate) fn configured_repository_identity(
    repository: &ProjectRepository,
) -> Result<Option<RepositoryIdentity>> {
    if let Some(source) = repository.github.as_deref() {
        return Ok(RepositoryIdentity::from_remote(source));
    }
    repository.local.as_deref().map(root_identity).transpose()
}

/// Reuse an exact configured bundle or synthesize one from all detected roots.
pub fn resolve_bundle(
    config: &Config,
    cwd: &Path,
    targets: &SessionEditTargets,
    requested_bundle: Option<&str>,
) -> Result<BundleResolution> {
    let cwd_root = git_root_for_path(cwd)?.context("session cwd is not in a Git worktree")?;
    // A linked worktree stands in for its main repository, so bundles are named
    // after and point at the main working tree.
    let cwd_root = main_worktree_root(&cwd_root)?;
    let primary_identity = root_identity(&cwd_root)?;
    let detected = targets
        .git_roots
        .iter()
        .map(|root| root_identity(root))
        .collect::<Result<BTreeSet<_>>>()?;

    if let Some(bundle_id) = requested_bundle {
        let bundle = config
            .bundles
            .get(bundle_id)
            .with_context(|| format!("unknown bundle {bundle_id:?}"))?;
        ensure!(
            bundle_matches(bundle, &detected, &primary_identity)?,
            "bundle {bundle_id:?} does not exactly match the session's edited Git roots and cwd primary repository"
        );
        return Ok(BundleResolution::Existing(bundle_id.to_owned()));
    }
    for (id, bundle) in &config.bundles {
        if bundle_matches(bundle, &detected, &primary_identity)? {
            return Ok(BundleResolution::Existing(id.clone()));
        }
    }

    let primary_name = cwd_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repository");
    let bundle_id = unique_bundle_id(config, &setup_style_id(primary_name));
    let mut used_ids = BTreeSet::new();
    let mut repositories = Vec::new();
    let mut primary_repo = None;
    let mut roots = targets
        .git_roots
        .iter()
        .map(|root| main_worktree_root(root))
        .collect::<Result<Vec<_>>>()?;
    roots.sort_by_key(|root| root != &cwd_root);
    // Checkouts and worktrees of one repository share an identity. Keep the
    // first root per identity so the cwd repository stays primary.
    let mut used_identities = BTreeSet::new();
    for root in roots {
        if !used_identities.insert(root_identity(&root)?) {
            continue;
        }
        let base = setup_style_id(
            root.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("repository"),
        );
        let mut id = base.clone();
        for suffix in 2_u32.. {
            if used_ids.insert(id.clone()) {
                break;
            }
            id = format!("{base}-{suffix}");
        }
        if root == cwd_root {
            primary_repo = Some(id.clone());
        }
        repositories.push(ProjectRepository {
            id: id.clone(),
            local: Some(root),
            github: None,
            destination: PathBuf::from(id),
            git_ref: None,
        });
    }
    Ok(BundleResolution::Synthesized {
        id: bundle_id,
        bundle: ProjectBundle {
            primary_repo: primary_repo.context("detected roots omitted the cwd repository")?,
            repositories,
        },
    })
}

pub(crate) fn bundle_matches(
    bundle: &ProjectBundle,
    detected: &BTreeSet<RepositoryIdentity>,
    primary: &RepositoryIdentity,
) -> Result<bool> {
    let identities = bundle
        .repositories
        .iter()
        .map(configured_repository_identity)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<BTreeSet<_>>();
    Ok(identities.len() == bundle.repositories.len()
        && &identities == detected
        && bundle
            .primary()
            .map(configured_repository_identity)
            .transpose()?
            .flatten()
            .as_ref()
            == Some(primary))
}

/// Return the matching configured bundle for an origin. It accepts setup's
/// `owner/repository` shorthand as well as normal GitHub remote URLs.
pub fn configured_bundle_for_origin(config: &Config, origin: &GithubRepository) -> Option<String> {
    config.bundles.iter().find_map(|(id, bundle)| {
        if bundle.repositories.len() != 1 {
            return None;
        }
        let primary = bundle.primary()?;
        let configured = github_repository_from_origin(primary.github.as_deref()?)?;
        same_github_repository(&configured, origin).then(|| id.clone())
    })
}

pub fn configured_bundle_for_local(config: &Config, local: &Path) -> Option<String> {
    let local = mj_core::local_git::canonical_repository(local).ok()?;
    config.bundles.iter().find_map(|(id, bundle)| {
        if bundle.repositories.len() != 1 {
            return None;
        }
        let configured = bundle.primary()?.local.as_ref()?;
        let configured = mj_core::local_git::canonical_repository(configured).ok()?;
        (configured == local).then(|| id.clone())
    })
}

pub(super) fn same_github_repository(left: &GithubRepository, right: &GithubRepository) -> bool {
    left.owner.eq_ignore_ascii_case(&right.owner)
        && left.repository.eq_ignore_ascii_case(&right.repository)
}

pub(crate) fn setup_style_id(value: &str) -> String {
    let mut id = value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        .take(64)
        .collect::<String>();
    if id.is_empty() || matches!(id.as_str(), "." | "..") {
        id = "repository".into();
    }
    id
}

pub(crate) fn unique_bundle_id(config: &Config, base: &str) -> String {
    if !config.bundles.contains_key(base) {
        return base.into();
    }
    let base = format!("import-{base}");
    if !config.bundles.contains_key(&base) {
        return base;
    }
    for suffix in 2_u32.. {
        let candidate = format!("{base}-{suffix}");
        if !config.bundles.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("u32 bundle suffixes are finite")
}

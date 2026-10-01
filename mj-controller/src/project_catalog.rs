//! Daemon-owned project discovery. Reads are immediate; refreshes coalesce and
//! run off the event loop. The durable catalog owns canonical project IDs.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use mj_core::config::{Config, ProjectBundle, ProjectRepository, TargetTemplate};
use mj_core::project_catalog::{ProjectCatalogStatus, ProjectCatalogView, ProjectLocation};
use mj_core::repository::{ProjectBundleSnapshot, RepositoryIdentity, ResolvedDirectory};
use mj_core::targets::{CancellableProcessExecutor, CommandExecutor};
use tokio_util::sync::CancellationToken;

use crate::{controller::RemoteGitExecutor, database};

pub(crate) fn snapshot(
    bundle: &ProjectBundle,
    executor: &impl CommandExecutor,
    launch: bool,
) -> Result<ProjectBundleSnapshot> {
    let mut project = ProjectBundleSnapshot {
        bundle: bundle.clone(),
        identities: BTreeMap::new(),
        network_sources: BTreeMap::new(),
    };
    for repository in &bundle.repositories {
        let identity = if let Some(source) = &repository.github {
            RepositoryIdentity::from_remote(source).context("invalid project remote")?
        } else {
            mj_core::repository::local_identity(
                repository
                    .local
                    .as_deref()
                    .context("project source is missing")?,
                executor,
            )?
        };
        project.identities.insert(repository.id.clone(), identity);
        if launch {
            project.network_sources.insert(
                repository.id.clone(),
                mj_core::remote_git::resolve_repository(repository, executor)?,
            );
        }
    }
    project.key()?;
    Ok(project)
}

pub(crate) fn resolve_directory(
    target: &TargetTemplate,
    path: &Path,
    executor: &impl CommandExecutor,
) -> Result<(String, ResolvedDirectory)> {
    match target {
        TargetTemplate::LocalBare => Ok((
            "local".into(),
            mj_core::repository::resolve_directory(path, None, executor)?,
        )),
        TargetTemplate::SshBare { ssh, .. } => {
            let host = mj_core::config::project_history_host(target)
                .context("project host is missing")?
                .to_owned();
            let resolved = mj_core::repository::resolve_directory(
                path,
                Some(&host),
                &RemoteGitExecutor {
                    executor,
                    ssh: crate::targets::SshTarget::from(ssh),
                },
            )?;
            Ok((host, resolved))
        }
        _ => anyhow::bail!("directory projects require a bare target"),
    }
}

fn directory_snapshot(resolved: &ResolvedDirectory) -> ProjectBundleSnapshot {
    let id = crate::import::setup_style_id(&resolved.identity.name());
    ProjectBundleSnapshot {
        bundle: ProjectBundle {
            primary_repo: id.clone(),
            repositories: vec![ProjectRepository {
                id: id.clone(),
                github: None,
                local: Some(resolved.checkout_root.clone()),
                destination: id.clone().into(),
                git_ref: None,
            }],
        },
        identities: BTreeMap::from([(id, resolved.identity.clone())]),
        network_sources: BTreeMap::new(),
    }
}

pub(crate) fn accept_directory(
    target: &TargetTemplate,
    directory: &Path,
    executor: &impl CommandExecutor,
) -> Result<(String, ProjectBundleSnapshot)> {
    let (host, resolved) = resolve_directory(target, directory, executor)?;
    let project = directory_snapshot(&resolved);
    let id = database::store_directory_project(&project)?;
    database::store_project_location(&ProjectLocation {
        host,
        directory: directory.to_owned(),
        checkout_root: resolved.checkout_root,
        repository_root: resolved.repository_root,
        identity: resolved.identity,
        seen_at: chrono::Utc::now().to_rfc3339(),
    })?;
    Ok((id, project))
}

/// Resolve changed TOML inputs only. Previously accepted definitions are used
/// for unchanged inputs, including sources temporarily unavailable offline.
fn reconcile_config(
    config: &Config,
    executor: &impl CommandExecutor,
    errors: &mut Vec<String>,
) -> Result<()> {
    let catalog = database::read_project_catalog()?;
    let state = database::load_state()?;
    let mut configured = config.bundles.iter().collect::<Vec<_>>();
    configured.sort_by(|(a, _), (b, _)| {
        let count = |id: &str| {
            state
                .sessions
                .values()
                .filter(|session| session.bundle_id == id)
                .count()
        };
        count(b).cmp(&count(a)).then(a.cmp(b))
    });
    for (id, bundle) in configured {
        ensure!(
            !executor.cancellation_requested(),
            "project discovery cancelled"
        );
        let existing = catalog
            .projects
            .iter()
            .find(|project| &project.bundle_id == id && &project.project.bundle == bundle);
        let resolved = existing
            .filter(|project| {
                !project.project.network_sources.is_empty()
                    || !state.sessions.values().any(|session| {
                        session.bundle_id == *id
                            && session.project.is_none()
                            && session.project_directory.is_none()
                    })
            })
            .map(|project| Ok(project.project.clone()))
            .unwrap_or_else(|| {
                snapshot(
                    bundle,
                    executor,
                    state.sessions.values().any(|session| {
                        session.bundle_id == *id
                            && session.project.is_none()
                            && session.project_directory.is_none()
                    }),
                )
            });
        match resolved {
            Ok(project) => {
                if !catalog.projects.iter().any(|entry| entry.bundle_id == *id) {
                    migrate_memory(
                        &project,
                        Some(bundle),
                        &state
                            .sessions
                            .values()
                            .filter(|session| {
                                session.bundle_id == *id && session.project_directory.is_none()
                            })
                            .collect::<Vec<_>>(),
                    )?;
                }
                database::store_catalog_project(id, &project, true)?;
            }
            Err(error)
                if error
                    .downcast_ref::<mj_core::repository::RepositoryUnavailable>()
                    .is_some() =>
            {
                tracing::debug!(project = %id, %error, "configured project is unavailable");
            }
            Err(error) => errors.push(format!("Project {id}: {error:#}")),
        }
    }
    // Aliases are durable before deleting duplicate TOML entries. Compare the
    // fresh definition so concurrent manual edits are never removed.
    for (id, canonical, project) in database::pending_project_aliases()? {
        ensure!(
            !executor.cancellation_requested(),
            "project discovery cancelled"
        );
        let _commit = crate::upgrade::activity_unless_draining("project consolidation config")?;
        Config::update(|fresh| {
            if fresh.bundles.get(&id) == Some(&project.bundle) {
                if !fresh.bundles.contains_key(&canonical) {
                    let saved = database::read_project_catalog()?
                        .projects
                        .into_iter()
                        .find(|entry| entry.bundle_id == canonical)
                        .context("alias canonical project is missing")?;
                    fresh
                        .bundles
                        .insert(canonical.clone(), saved.project.bundle);
                }
                fresh.bundles.remove(&id);
            }
            Ok(())
        })?;
        database::finish_project_alias(&id)?;
    }
    Ok(())
}

/// Record a local directory found in history in the catalog. Discovery never
/// writes `config.toml`: saved bundles are the ones the user created, and a
/// discovered project becomes one only when the user chooses it.
fn discover_local(path: &Path, executor: &impl CommandExecutor) -> Result<()> {
    accept_directory(&TargetTemplate::LocalBare, path, executor)?;
    Ok(())
}

fn historical_discovery_error(result: Result<()>, path: &Path) -> Option<String> {
    result.err().and_then(|error| {
        if error.downcast_ref::<mj_core::repository::RepositoryUnavailable>().is_some() {
            tracing::debug!(directory = %path.display(), %error, "historical project is unavailable");
            None
        } else {
            Some(format!("{}: {error:#}", path.display()))
        }
    })
}

fn refresh(executor: &impl CommandExecutor, retry: bool) -> Result<Vec<String>> {
    let mut errors = Vec::new();
    let config = Config::load()?;
    reconcile_config(&config, executor, &mut errors)?;
    let mut homes = BTreeSet::new();
    for profile in config.profiles.values().filter(|profile| profile.enabled) {
        let home = std::fs::canonicalize(&profile.home).unwrap_or_else(|_| profile.home.clone());
        let harness = serde_json::to_string(&profile.kind)?;
        if !homes.insert((harness.clone(), home.clone()))
            || database::project_seeded(&harness, &home)?
        {
            continue;
        }
        ensure!(
            !executor.cancellation_requested(),
            "project discovery cancelled"
        );
        match crate::import::recent_project_directories(profile.kind, &home, 10, executor) {
            Ok(seed) => {
                for (path, error) in seed.errors {
                    database::seed_failure(&harness, &home, &path, true, Some(error.clone()))?;
                    errors.push(error);
                }
                for path in seed.directories {
                    ensure!(
                        !executor.cancellation_requested(),
                        "project discovery cancelled"
                    );
                    if let Some(error) =
                        historical_discovery_error(discover_local(&path, executor), &path)
                    {
                        database::seed_failure(&harness, &home, &path, false, Some(error.clone()))?;
                        errors.push(error);
                    }
                }
                ensure!(
                    !executor.cancellation_requested(),
                    "project discovery cancelled"
                );
                database::finish_project_seed(&harness, &home)?;
            }
            Err(error) => errors.push(format!(
                "{} profile {}: {error:#}",
                profile.kind.display_name(),
                home.display()
            )),
        }
    }
    if retry {
        for (harness, home, path, source_file) in database::seed_failures()? {
            ensure!(
                !executor.cancellation_requested(),
                "project discovery cancelled"
            );
            let result = if source_file {
                let kind = serde_json::from_str(&harness)?;
                crate::import::native_project_directory(&path, kind, executor)
                    .and_then(|directory| discover_local(&directory, executor))
            } else {
                discover_local(&path, executor)
            };
            let error = historical_discovery_error(result, &path);
            if let Some(error) = &error {
                errors.push(error.clone());
            }
            database::seed_failure(&harness, &home, &path, source_file, error)?;
        }
    }
    loop {
        let changes = database::project_discovery_changes(retry)?;
        if changes.is_empty() {
            break;
        }
        // Failed candidates are visited once per explicit retry, not in every
        // batch of the same pass.
        for change in &changes {
            ensure!(
                !executor.cancellation_requested(),
                "project discovery cancelled"
            );
            let path = change
                .managed_worktree
                .as_ref()
                .map(|worktree| worktree.source_project_directory.as_path())
                .unwrap_or(&change.directory);
            let result = (|| {
                let known = database::load_state()?
                    .sessions
                    .get(&change.session_id)
                    .cloned();
                let recorded = known
                    .as_ref()
                    .and_then(|session| session.target_runtime.as_ref())
                    .and_then(|runtime| match &runtime.connection {
                        mj_core::state::TargetConnection::Local => Some(TargetTemplate::LocalBare),
                        mj_core::state::TargetConnection::Ssh { ssh } => {
                            Some(TargetTemplate::SshBare {
                                ssh: ssh.clone(),
                                permissions: mj_core::config::PermissionMode::Guardian,
                                workspace_prefix: "/tmp/mj-workspaces".into(),
                            })
                        }
                        _ => None,
                    });
                let target = recorded
                    .as_ref()
                    .or_else(|| config.targets.get(&change.target_id))
                    .context("historical project target is unavailable")?;
                let (id, project) = if let Some(session) = &known
                    && let Some(project) = &session.project
                {
                    (session.bundle_id.clone(), project.clone())
                } else {
                    accept_directory(target, path, executor)?
                };
                if let Some(session) = known.as_ref().filter(|session| session.project.is_none()) {
                    migrate_memory(&project, None, &[session])?;
                }
                database::bind_session_project(&change.session_id, &id, &project)?;
                if matches!(target, TargetTemplate::LocalBare) {
                    discover_local(path, executor)?;
                }
                Ok::<_, anyhow::Error>(())
            })();
            ensure!(
                !executor.cancellation_requested(),
                "project discovery cancelled"
            );
            let error = historical_discovery_error(result, path);
            if let Some(error) = &error {
                errors.push(error.clone());
            }
            database::finish_project_discovery(change.sequence, error)?;
        }
        if retry || changes.len() < 128 {
            break;
        }
    }
    errors.extend(database::discovery_errors()?);
    errors.sort();
    errors.dedup();
    Ok(errors)
}

fn migrate_memory(
    project: &ProjectBundleSnapshot,
    bundle: Option<&ProjectBundle>,
    sessions: &[&mj_core::state::SessionRecord],
) -> Result<()> {
    use mj_core::project_memory::{
        ProjectMemoryIdentity as Project, RepositoryMemoryIdentity as Repository,
    };
    let canonical = project.memory_identity()?.key()?;
    let root = |key: &str| {
        mj_core::config::data_dir()
            .join("projects")
            .join(key)
            .join("memory")
    };
    let mut legacy = Vec::new();
    if let Some(bundle) = bundle {
        let identity = Repository::from_configured;
        let primary = identity(bundle.primary().context("legacy primary is missing")?)?;
        let members = bundle
            .repositories
            .iter()
            .map(identity)
            .collect::<Result<Vec<_>>>()?;
        legacy.push(Project::bundle(primary, members).key()?);
    }
    for session in sessions {
        legacy.push(Project::for_legacy_session(session, bundle, None)?.key()?);
    }
    for key in legacy {
        for conflict in
            mj_core::project_memory::merge_canonical_stores(&root(&key), &root(&canonical))?
        {
            tracing::warn!(%conflict,"project memory merge preserved conflicting documents");
        }
    }
    Ok(())
}

#[derive(Default)]
pub(crate) struct Catalog {
    requested: AtomicBool,
    retry: AtomicBool,
    notify: tokio::sync::Notify,
    status: Mutex<ProjectCatalogStatus>,
}

impl Catalog {
    pub(crate) fn request(&self, retry: bool) {
        if retry {
            self.retry.store(true, Ordering::Release);
        }
        self.requested.store(true, Ordering::Release);
        *self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = ProjectCatalogStatus::Refreshing;
        self.notify.notify_one();
    }

    pub(crate) fn view(&self) -> Result<ProjectCatalogView> {
        let mut view = database::read_project_catalog()?;
        for saved in &mut view.projects {
            for source in saved.project.network_sources.values_mut() {
                source.fetch_url = mj_core::remote_git::display_url(&source.fetch_url);
                source.push_urls = source
                    .push_urls
                    .iter()
                    .map(|url| mj_core::remote_git::display_url(url))
                    .collect();
            }
        }
        view.status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Ok(view)
    }

    pub(crate) async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        // Reconcile durable failures once at startup: an upgraded resolver or
        // restored checkout must recover without requiring a manual retry.
        self.request(true);
        loop {
            tokio::select! { _=shutdown.cancelled()=>break, _=self.notify.notified()=>{} }
            if !self.requested.swap(false, Ordering::AcqRel) {
                continue;
            }
            let retry = self.retry.swap(false, Ordering::AcqRel);
            let cancelled = Arc::new(AtomicBool::new(false));
            let executor = CancellableProcessExecutor::new(cancelled.clone())
                .with_deadline(Duration::from_secs(120));
            let mut task = tokio::task::spawn_blocking(move || refresh(&executor, retry));
            let result = tokio::select! {
                result=&mut task=>result.context("project discovery task panicked").and_then(|result|result),
                _=shutdown.cancelled()=>{
                    cancelled.store(true,Ordering::Release);
                    match tokio::time::timeout(Duration::from_secs(2), &mut task).await {
                        Ok(Ok(Ok(_))) => {}
                        Ok(Ok(Err(error))) => tracing::warn!(%error,"project discovery stopped during cancellation"),
                        Ok(Err(error)) => tracing::warn!(%error,"project discovery cancellation failed"),
                        Err(_) => {
                            tracing::warn!("project discovery cleanup exceeded two seconds; unfinished discovery will resume at startup");
                            tokio::spawn(async move {
                                match task.await {
                                    Ok(Ok(_)) => {}
                                    Ok(Err(error)) => tracing::warn!(%error,"cancelled project discovery failed"),
                                    Err(error) => tracing::warn!(%error,"cancelled project discovery task failed"),
                                }
                            });
                        }
                    }
                    break;
                }
            };
            let status = match result {
                Ok(errors) if errors.is_empty() => ProjectCatalogStatus::Ready,
                Ok(errors) => ProjectCatalogStatus::Failed { errors },
                Err(error) => ProjectCatalogStatus::Failed {
                    errors: vec![format!("{error:#}")],
                },
            };
            if let ProjectCatalogStatus::Failed { errors } = &status {
                for error in errors {
                    tracing::warn!(%error,"project discovery failed");
                }
            }
            *self
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                if self.requested.load(Ordering::Acquire) {
                    ProjectCatalogStatus::Refreshing
                } else {
                    status
                };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::config::{HarnessKind, HarnessProfile};
    use mj_core::targets::{CommandSpec, ProcessExecutor};

    #[test]
    fn unavailable_history_is_retired_and_renamed_repositories_keep_memory() {
        const CHILD: &str = "MJ_DISCOVERY_RECOVERY_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let mut command = CommandSpec::new(std::env::current_exe().unwrap().to_string_lossy(), ["--exact", "project_catalog::tests::unavailable_history_is_retired_and_renamed_repositories_keep_memory", "--nocapture"])
                .purpose("isolated discovery recovery regression");
            command.env.extend([
                (CHILD.into(), "1".into()),
                (
                    "MJ_CONFIG_DIR".into(),
                    directory
                        .path()
                        .join("config")
                        .to_string_lossy()
                        .into_owned(),
                ),
                (
                    "MJ_DATA_DIR".into(),
                    directory.path().join("data").to_string_lossy().into_owned(),
                ),
            ]);
            let output = ProcessExecutor.execute(&command).unwrap();
            assert_eq!(
                output.status,
                0,
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        use mj_core::project_memory::{
            ProjectMemoryIdentity, ProjectMemorySnapshot, ProjectMemoryStore,
            RepositoryMemoryIdentity,
        };
        let root = tempfile::tempdir().unwrap();
        let _writer = database::install_isolated_test_writer();
        let checkout = root.path().join("healthy-checkout");
        std::fs::create_dir(&checkout).unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "remote",
                "add",
                "origin",
                "https://github.com/Example/new-name.git",
            ],
        ] {
            let mut command = CommandSpec::new("git", ["-C", &checkout.to_string_lossy()])
                .purpose("initialize discovery recovery checkout");
            command.args.extend(args.into_iter().map(str::to_owned));
            assert_eq!(ProcessExecutor.execute(&command).unwrap().status, 0);
        }
        let legacy_bundle = ProjectBundle {
            primary_repo: "old-id".into(),
            repositories: vec![ProjectRepository {
                id: "old-id".into(),
                github: Some("Example/legacy-repository".into()),
                local: None,
                destination: "old-id".into(),
                git_ref: None,
            }],
        };
        Config::update(|config| {
            config.profiles.clear();
            config.bundles = BTreeMap::from([("old-project".into(), legacy_bundle.clone())]);
            config.targets = BTreeMap::from([("localhost".into(), TargetTemplate::LocalBare)]);
            Ok(())
        })
        .unwrap();
        let legacy_key = ProjectMemoryIdentity::bundle(
            RepositoryMemoryIdentity::from_configured(&legacy_bundle.repositories[0]).unwrap(),
            vec![
                RepositoryMemoryIdentity::from_configured(&legacy_bundle.repositories[0]).unwrap(),
            ],
        )
        .key()
        .unwrap();
        let legacy_root = mj_core::config::data_dir()
            .join("projects")
            .join(&legacy_key)
            .join("memory");
        let bundle_memory = ProjectMemorySnapshot {
            files: BTreeMap::from([(
                "/bundle.md".into(),
                "Unrelated configured project memory".into(),
            )]),
        };
        ProjectMemoryStore::new(&legacy_root)
            .install_snapshot(&bundle_memory)
            .unwrap();
        let mut record = crate::database::test_session("healthy", "old-project");
        record.project_directory = Some(checkout.clone());
        record.target_template_id = "localhost".into();
        record.target = Some(mj_core::state::TargetLocator::LocalBare {
            worker_root: root.path().join("workers").join(&record.id),
        });
        let raw_key =
            ProjectMemoryIdentity::for_legacy_session(&record, Some(&legacy_bundle), None)
                .unwrap()
                .key()
                .unwrap();
        let raw_root = mj_core::config::data_dir()
            .join("projects")
            .join(raw_key)
            .join("memory");
        let memory = ProjectMemorySnapshot {
            files: BTreeMap::from([("/notes.md".into(), "Keep this raw-session memory".into())]),
        };
        ProjectMemoryStore::new(&raw_root)
            .install_snapshot(&memory)
            .unwrap();
        database::save_session(&record).unwrap();

        let missing = root.path().join("deleted-checkout");
        let nongit = root.path().join("not-a-repository");
        std::fs::create_dir(&nongit).unwrap();
        for (index, path) in [&missing, &nongit].into_iter().enumerate() {
            database::seed_failure(
                "\"codex\"",
                root.path(),
                path,
                false,
                Some("old unavailable error".into()),
            )
            .unwrap();
            let mut record =
                crate::database::test_session(&format!("stale-{index}"), "old-project");
            record.project_directory = Some(path.clone());
            record.target_template_id = "localhost".into();
            database::save_session(&record).unwrap();
        }
        let executor = CancellableProcessExecutor::with_timeout(Duration::from_secs(30));
        for change in database::project_discovery_changes(false).unwrap() {
            database::finish_project_discovery(
                change.sequence,
                Some("previous discovery failure".into()),
            )
            .unwrap();
        }
        // All candidates are behind the durable progress frontier. Starting a
        // new daemon's catalog must retry them without a user pressing Retry.
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let catalog = Arc::new(Catalog::default());
            let shutdown = CancellationToken::new();
            let task = tokio::spawn(catalog.clone().run(shutdown.clone()));
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    let status = catalog.view().unwrap().status;
                    if !matches!(status, ProjectCatalogStatus::Refreshing) {
                        assert_eq!(status, ProjectCatalogStatus::Ready);
                        break;
                    }
                }
            })
            .await
            .unwrap();
            shutdown.cancel();
            task.await.unwrap();
        });
        assert!(database::discovery_errors().unwrap().is_empty());
        assert!(database::seed_failures().unwrap().is_empty());
        assert!(
            database::project_discovery_changes(false)
                .unwrap()
                .is_empty()
        );
        let state = database::load_state().unwrap();
        assert_eq!(
            state.sessions.len(),
            3,
            "retiring candidates preserves session history"
        );
        let accepted = state.sessions["healthy"].project.as_ref().unwrap();
        assert_eq!(accepted.bundle.primary_repo, "new-name");
        let canonical_root = mj_core::config::data_dir()
            .join("projects")
            .join(accepted.memory_identity().unwrap().key().unwrap())
            .join("memory");
        assert_eq!(
            ProjectMemoryStore::new(&canonical_root).snapshot().unwrap(),
            memory
        );
        assert_eq!(
            mj_core::project_memory::resolve_canonical_root(&raw_root).unwrap(),
            canonical_root
        );
        assert_eq!(
            ProjectMemoryStore::new(&legacy_root).snapshot().unwrap(),
            bundle_memory
        );
        // An explicit configured-project migration uses its old source,
        // even when the new snapshot has a different repository ID.
        migrate_memory(accepted, Some(&legacy_bundle), &[]).unwrap();
        let mut merged = memory.clone();
        merged.files.extend(bundle_memory.files);
        assert_eq!(
            ProjectMemoryStore::new(&canonical_root).snapshot().unwrap(),
            merged
        );
        assert_eq!(
            mj_core::project_memory::resolve_canonical_root(&legacy_root).unwrap(),
            canonical_root
        );

        // A location already accepted into the catalog must be checked again:
        // deleting its Git metadata cannot leave it falsely discoverable.
        std::fs::remove_dir_all(checkout.join(".git")).unwrap();
        let error = discover_local(&checkout, &executor).unwrap_err();
        assert!(
            error
                .downcast_ref::<mj_core::repository::RepositoryUnavailable>()
                .is_some()
        );
        assert!(
            historical_discovery_error(Err(anyhow::anyhow!("permission denied")), &checkout)
                .is_some()
        );
        let malformed = root.path().join("malformed-native-session.jsonl");
        std::fs::write(&malformed, "{\"cwd\": broken JSON\n").unwrap();
        database::seed_failure(
            "\"codex\"",
            root.path(),
            &malformed,
            true,
            Some("old parse failure".into()),
        )
        .unwrap();
        let errors = refresh(&executor, true).unwrap();
        assert_eq!(
            errors.len(),
            1,
            "real discovery failures must remain visible: {errors:?}"
        );
        assert!(errors[0].contains("malformed-native-session.jsonl"));
    }

    #[test]
    fn discovery_seeds_ten_sessions_once_and_refreshes_only_mjolnir_changes() {
        const CHILD: &str = "MJ_PROJECT_CATALOG_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            let mut command=CommandSpec::new(std::env::current_exe().unwrap().to_string_lossy(),["--exact","project_catalog::tests::discovery_seeds_ten_sessions_once_and_refreshes_only_mjolnir_changes","--nocapture"]).purpose("isolated project catalog discovery regression");
            command.env.extend([
                (CHILD.into(), "1".into()),
                (
                    "MJ_CONFIG_DIR".into(),
                    directory
                        .path()
                        .join("config")
                        .to_string_lossy()
                        .into_owned(),
                ),
                (
                    "MJ_DATA_DIR".into(),
                    directory.path().join("data").to_string_lossy().into_owned(),
                ),
            ]);
            let output = ProcessExecutor.execute(&command).unwrap();
            assert_eq!(
                output.status,
                0,
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let _writer = database::install_isolated_test_writer();
        let home = root.path().join("codex");
        std::fs::create_dir_all(&home).unwrap();
        let index = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
        index.execute_batch("CREATE TABLE threads(id TEXT,rollout_path TEXT,updated_at INTEGER,name TEXT,title TEXT,cwd TEXT,git_branch TEXT,history_mode TEXT,archived INTEGER,source TEXT,preview TEXT)").unwrap();
        let initialize = |path: &Path| {
            std::fs::create_dir_all(path).unwrap();
            let output = ProcessExecutor
                .execute(
                    &CommandSpec::new("git", ["-C", &path.to_string_lossy(), "init", "-q"])
                        .purpose("initialize discovery fixture"),
                )
                .unwrap();
            assert_eq!(output.status, 0);
        };
        for i in 0..12 {
            let project = root.path().join(format!("project-{i:02}"));
            initialize(&project);
            let log = home.join(format!("session-{i}.jsonl"));
            std::fs::write(&log, "conversation deliberately unreadable").unwrap();
            index.execute("INSERT INTO threads VALUES(?1,?2,?3,'title','title',?4,'main','paginated',0,'cli','preview')",rusqlite::params![format!("session-{i}"),log.to_str().unwrap(),i,project.to_str().unwrap()]).unwrap();
        }
        let profile = |kind, home| HarnessProfile {
            enabled: true,
            kind,
            home,
            environment: Default::default(),
            context_window_bytes: None,
            subagents: Default::default(),
            guardian_review_model: None,
        };
        Config::update(|config| {
            config.profiles = BTreeMap::from([
                ("codex".into(), profile(HarnessKind::Codex, home.clone())),
                (
                    "missing-claude".into(),
                    profile(HarnessKind::Claude, root.path().join("missing")),
                ),
            ]);
            config.targets = BTreeMap::from([("localhost".into(), TargetTemplate::LocalBare)]);
            Ok(())
        })
        .unwrap();
        let errors = refresh(
            &CancellableProcessExecutor::with_timeout(Duration::from_secs(30)),
            false,
        )
        .unwrap();
        assert!(errors.iter().any(|error| error.contains("missing")));
        let view = database::read_project_catalog().unwrap();
        assert_eq!(view.projects.len(), 10);
        assert_eq!(view.locations.len(), 10);
        // Discovered projects belong to the catalog. config.toml holds only
        // the bundles the user saved.
        assert!(Config::load().unwrap().bundles.is_empty());
        assert!(
            !view
                .locations
                .iter()
                .any(|location| location.directory.ends_with("project-00")
                    || location.directory.ends_with("project-01"))
        );
        drop(index);
        std::fs::write(
            home.join("state_5.sqlite"),
            "broken index that must never be scanned again",
        )
        .unwrap();
        refresh(
            &CancellableProcessExecutor::with_timeout(Duration::from_secs(30)),
            false,
        )
        .unwrap();
        assert_eq!(
            database::read_project_catalog().unwrap().locations.len(),
            10
        );
        let path = root.path().join("new-mj-project");
        initialize(&path);
        let selected = path.join("src");
        std::fs::create_dir(&selected).unwrap();
        let mut record = crate::database::test_session("new-mj-session", "legacy-raw");
        record.project_directory = Some(selected.clone());
        record.target_template_id = "localhost".into();
        record.last_profile = "codex".into();
        database::save_session(&record).unwrap();
        refresh(
            &CancellableProcessExecutor::with_timeout(Duration::from_secs(30)),
            false,
        )
        .unwrap();
        let loaded = database::load_state().unwrap().sessions[&record.id].clone();
        assert_eq!(loaded.project_directory, Some(selected));
        assert_eq!(
            loaded.project.as_ref().unwrap().identities.values().next(),
            Some(&RepositoryIdentity::Local(path.canonicalize().unwrap()))
        );
        assert_eq!(
            database::read_project_catalog().unwrap().locations.len(),
            11
        );
        assert!(Config::load().unwrap().bundles.is_empty());
        assert!(
            database::project_discovery_changes(false)
                .unwrap()
                .is_empty()
        );
    }
}

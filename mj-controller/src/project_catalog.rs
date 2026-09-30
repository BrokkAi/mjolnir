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
                            .filter(|session| session.bundle_id == *id)
                            .collect::<Vec<_>>(),
                    )?;
                }
                database::store_catalog_project(id, &project, true)?;
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

fn discover_local(path: &Path, executor: &impl CommandExecutor) -> Result<()> {
    let project = if let Some(location) = database::read_project_catalog()?
        .locations
        .into_iter()
        .find(|location| location.host == "local" && location.directory == path)
    {
        ensure!(
            path.is_dir(),
            "historical project directory is missing: {}",
            path.display()
        );
        directory_snapshot(&ResolvedDirectory {
            checkout_root: location.checkout_root,
            repository_root: location.repository_root,
            identity: location.identity,
        })
    } else {
        accept_directory(&TargetTemplate::LocalBare, path, executor)?.1
    };
    // A remote-less repository can be suggested for raw launch, but managed
    // launch already requires a network source. Keep its local definition.
    let catalog = database::read_project_catalog()?;
    let entry = catalog
        .projects
        .iter()
        .find(|entry| entry.project.key().ok() == project.key().ok())
        .context("discovered project was not stored")?;
    let id = entry.bundle_id.clone();
    let _commit = crate::upgrade::activity_unless_draining("project discovery config")?;
    Config::update(|config| {
        config
            .bundles
            .entry(id)
            .or_insert_with(|| entry.project.bundle.clone());
        Ok(())
    })?;
    Ok(())
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
                    if let Err(error) = discover_local(&path, executor) {
                        let error = format!("{}: {error:#}", path.display());
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
            let error = result
                .err()
                .map(|error| format!("{}: {error:#}", path.display()));
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
                    migrate_memory(&project, session.project_bundle(&config), &[session])?;
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
            let error = result
                .err()
                .map(|error| format!("{}: {error:#}", path.display()));
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
        let identity = |repo: &ProjectRepository| -> Result<Repository> {
            if let Some(local) = &repo.local {
                Ok(Repository::Local {
                    canonical_root: mj_core::local_git::main_worktree_root(local)
                        .unwrap_or_else(|_| local.clone()),
                })
            } else {
                Ok(project
                    .identities
                    .get(&repo.id)
                    .context("legacy memory repository is missing")?
                    .into())
            }
        };
        let primary = identity(bundle.primary().context("legacy primary is missing")?)?;
        let mut members = bundle
            .repositories
            .iter()
            .map(identity)
            .collect::<Result<Vec<_>>>()?;
        members.sort_by_key(|member| {
            serde_json::to_string(member).expect("memory identity serializes")
        });
        members.dedup();
        legacy.push(Project::Bundle { primary, members }.key()?);
    }
    for session in sessions {
        if let Some(path) = session
            .managed_worktree
            .as_ref()
            .map(|worktree| &worktree.source_repository)
            .or(session.project_directory.as_ref())
        {
            let identity = if session.target_runtime.as_ref().is_some_and(|runtime| {
                matches!(runtime.connection, mj_core::state::TargetConnection::Local)
            }) || session.target_template_id == "localhost"
            {
                Repository::Local {
                    canonical_root: std::fs::canonicalize(path).unwrap_or_else(|_| path.clone()),
                }
            } else {
                Repository::Remote {
                    target: session.target_template_id.clone(),
                    canonical_root: path.clone(),
                }
            };
            legacy.push(
                Project::Repository {
                    repository: identity,
                }
                .key()?,
            );
        }
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
        self.request(false);
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
        assert!(
            database::project_discovery_changes(false)
                .unwrap()
                .is_empty()
        );
    }
}

//! Durable catalog decisions share the controller's serialized database writer.

use super::*;
use mj_core::project_catalog::{ProjectCatalogView, ProjectLocation, SavedProject};
use mj_core::repository::ProjectBundleSnapshot;

#[derive(Debug, Clone)]
pub(crate) struct ProjectDiscoveryChange {
    pub sequence: i64,
    pub session_id: String,
    pub directory: PathBuf,
    pub managed_worktree: Option<ManagedWorktree>,
    pub target_id: String,
}

pub(crate) fn read_project_catalog() -> Result<ProjectCatalogView> {
    read_project_catalog_from(&open_reader(&database_path())?)
}

pub(super) fn read_project_catalog_from(connection: &Connection) -> Result<ProjectCatalogView> {
    let mut view = ProjectCatalogView::default();
    let mut statement = connection.prepare(
        "SELECT bundle_id,snapshot_json FROM project_catalog WHERE hidden=0 ORDER BY bundle_id",
    )?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (bundle_id, json) = row?;
        let project: ProjectBundleSnapshot = serde_json::from_str(&json)?;
        view.projects.push(SavedProject {
            bundle_id,
            name: project.name(),
            project,
        });
    }
    let mut statement = connection.prepare(
        "SELECT host,directory,checkout_root,repository_root,identity_json,seen_at
         FROM project_locations ORDER BY seen_at DESC,host,directory",
    )?;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Vec<u8>>(2)?,
            row.get::<_, Vec<u8>>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })? {
        let (host, directory, checkout_root, repository_root, identity, seen_at) = row?;
        view.locations.push(ProjectLocation {
            host,
            directory: blob_to_path(&directory),
            checkout_root: blob_to_path(&checkout_root),
            repository_root: blob_to_path(&repository_root),
            identity: serde_json::from_str(&identity)?,
            seen_at,
        });
    }
    Ok(view)
}

pub(crate) fn store_project_location(location: &ProjectLocation) -> Result<()> {
    ensure!(
        !location.host.trim().is_empty()
            && location.directory.is_absolute()
            && location.checkout_root.is_absolute()
            && location.repository_root.is_absolute(),
        "project location on {:?} must contain absolute directory, checkout, and repository paths",
        location.host
    );
    let location = location.clone();
    submit_database_write("store_project_location", move |_| {
        let _commit = crate::upgrade::activity_unless_draining("project location catalog")?;
        let connection = open(&database_path())?;
        connection.execute("INSERT INTO project_locations(host,directory,checkout_root,repository_root,identity_json,seen_at)
            VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(host,directory) DO UPDATE SET
            checkout_root=excluded.checkout_root,repository_root=excluded.repository_root,
            identity_json=excluded.identity_json,seen_at=max(project_locations.seen_at,excluded.seen_at)",
            params![location.host,path_to_blob(&location.directory),path_to_blob(&location.checkout_root),path_to_blob(&location.repository_root),serde_json::to_string(&location.identity)?,location.seen_at])?;
        Ok(())
    })
}

/// Existing choices win after first reconciliation. Callers order new entries
/// by reference count then ID so a fresh catalog chooses the least disruptive ID.
pub(crate) fn store_catalog_project(
    bundle_id: &str,
    project: &ProjectBundleSnapshot,
    configured: bool,
) -> Result<String> {
    let bundle_id = bundle_id.to_owned();
    let project = project.clone();
    submit_database_write("store_catalog_project", move |_| {
        let _commit = crate::upgrade::activity_unless_draining("configured project catalog")?;
        store_catalog_project_at(&database_path(), &bundle_id, &project, configured)
    })
}

fn store_catalog_project_at(
    path: &Path,
    bundle_id: &str,
    project: &ProjectBundleSnapshot,
    configured: bool,
) -> Result<String> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let key = project.key()?;
    let snapshot = serde_json::to_string(project)?;
    // Reintroducing a removed alias as a different project must not redirect
    // newly created sessions to the old project. Existing workers already
    // have session-scoped aliases and retain their original context.
    let previous_alias: Option<String> = tx
        .query_row(
            "SELECT snapshot_json FROM project_aliases WHERE bundle_id=?1",
            [bundle_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(previous) = previous_alias
        && serde_json::from_str::<ProjectBundleSnapshot>(&previous)?.key()? != key
    {
        tx.execute(
            "DELETE FROM project_aliases WHERE bundle_id=?1",
            [bundle_id],
        )?;
    }
    retire_changed_project(&tx, bundle_id, &key)?;
    let canonical: Option<String> = tx
        .query_row(
            "SELECT bundle_id FROM project_catalog WHERE project_key=?1",
            [&key],
            |row| row.get(0),
        )
        .optional()?;
    let canonical = canonical.unwrap_or_else(|| bundle_id.to_owned());
    tx.execute(
        "INSERT INTO project_catalog(bundle_id,project_key,snapshot_json) VALUES(?1,?2,?3)
        ON CONFLICT(bundle_id) DO UPDATE SET snapshot_json=excluded.snapshot_json,hidden=0
        WHERE project_catalog.project_key=excluded.project_key AND project_catalog.bundle_id=?4",
        params![canonical, key, snapshot, bundle_id],
    )?;
    let stored_key: String = tx.query_row(
        "SELECT project_key FROM project_catalog WHERE bundle_id=?1",
        [&canonical],
        |row| row.get(0),
    )?;
    ensure!(
        stored_key == key,
        "project {canonical:?} changed repository membership; save it with a new project ID"
    );
    if canonical != bundle_id {
        tx.execute("INSERT INTO project_aliases(bundle_id,canonical_id,snapshot_json,config_pending) VALUES(?1,?2,?3,?4)
            ON CONFLICT(bundle_id) DO UPDATE SET canonical_id=excluded.canonical_id,
            snapshot_json=excluded.snapshot_json,config_pending=max(project_aliases.config_pending,excluded.config_pending)",params![bundle_id,canonical,snapshot,configured])?;
    }
    if configured {
        tx.execute(
            "UPDATE project_catalog SET hidden=0 WHERE bundle_id=?1",
            [&canonical],
        )?;
    }
    // Capture the old accepted definition before rebinding its context. Active
    // workers retain their own replica and repository IDs across this update.
    tx.execute(
        "UPDATE sessions SET project_json=?2 WHERE project_json IS NULL AND project_directory IS NULL
        AND session_id IN (SELECT session_id FROM session_contexts WHERE bundle_id=?1)",
        params![bundle_id, snapshot],
    )?;
    if canonical != bundle_id {
        tx.execute("INSERT OR IGNORE INTO project_session_aliases(session_id,bundle_id) SELECT session_id,bundle_id FROM session_contexts WHERE bundle_id=?1",[bundle_id])?;
        tx.execute(
            "UPDATE session_contexts SET bundle_id=?2 WHERE bundle_id=?1",
            params![bundle_id, canonical],
        )?;
    }
    tx.commit()?;
    Ok(canonical)
}

/// An edited TOML ID can describe a new shape. Existing sessions retain the
/// old shape and a scoped alias, so their workers cannot write new-project history.
fn retire_changed_project(tx: &Transaction<'_>, id: &str, new_key: &str) -> Result<()> {
    let previous: Option<(String, String)> = tx
        .query_row(
            "SELECT project_key,snapshot_json FROM project_catalog WHERE bundle_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((key, snapshot)) = previous.filter(|(key, _)| key != new_key) else {
        return Ok(());
    };
    let base = format!("{id}-history");
    let mut retired = base.clone();
    for suffix in 2_u32.. {
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM project_catalog WHERE bundle_id=?1 UNION SELECT 1 FROM project_aliases WHERE bundle_id=?1)",[&retired],|row|row.get(0))?;
        if !exists {
            break;
        }
        retired = format!("{base}-{suffix}");
    }
    tx.execute("INSERT INTO project_catalog(bundle_id,project_key,snapshot_json,hidden) VALUES(?1,?2,?3,1)",params![retired,format!("retiring:{retired}"),snapshot])?;
    tx.execute("UPDATE sessions SET project_json=?2 WHERE project_json IS NULL AND project_directory IS NULL AND session_id IN (SELECT session_id FROM session_contexts WHERE bundle_id=?1)",params![id,snapshot])?;
    tx.execute("INSERT OR IGNORE INTO project_session_aliases(session_id,bundle_id) SELECT session_id,bundle_id FROM session_contexts WHERE bundle_id=?1",[id])?;
    tx.execute(
        "UPDATE session_contexts SET bundle_id=?2 WHERE bundle_id=?1",
        params![id, retired],
    )?;
    tx.execute(
        "UPDATE project_aliases SET canonical_id=?2 WHERE canonical_id=?1",
        params![id, retired],
    )?;
    tx.execute("DELETE FROM project_catalog WHERE bundle_id=?1", [id])?;
    tx.execute(
        "UPDATE project_catalog SET project_key=?2 WHERE bundle_id=?1",
        params![retired, key],
    )?;
    Ok(())
}

pub(crate) fn pending_project_aliases() -> Result<Vec<(String, String, ProjectBundleSnapshot)>> {
    let connection = open_reader(&database_path())?;
    let mut statement=connection.prepare("SELECT bundle_id,canonical_id,snapshot_json FROM project_aliases WHERE config_pending=1 ORDER BY bundle_id")?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .map(|row| {
            let (id, canonical, json) = row?;
            Ok((id, canonical, serde_json::from_str(&json)?))
        })
        .collect()
}

pub(crate) fn finish_project_alias(bundle_id: &str) -> Result<()> {
    let bundle_id = bundle_id.to_owned();
    submit_database_write("finish_project_alias", move |_| {
        let _commit = crate::upgrade::activity_unless_draining("project alias completion")?;
        open(&database_path())?.execute(
            "UPDATE project_aliases SET config_pending=0 WHERE bundle_id=?1",
            [bundle_id],
        )?;
        Ok(())
    })
}

pub(super) fn session_project_id(
    connection: &Connection,
    session_id: &str,
    bundle_id: &str,
) -> Result<String> {
    let scoped:Option<String>=connection.query_row("SELECT c.bundle_id FROM session_contexts c JOIN project_session_aliases a ON a.session_id=c.session_id WHERE c.session_id=?1 AND a.bundle_id=?2",params![session_id,bundle_id],|row|row.get(0)).optional()?;
    match scoped {
        Some(id) => Ok(id),
        None => canonical_project_id(connection, bundle_id),
    }
}

pub(super) fn canonical_project_id(connection: &Connection, bundle_id: &str) -> Result<String> {
    Ok(connection
        .query_row(
            "SELECT canonical_id FROM project_aliases WHERE bundle_id=?1",
            [bundle_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_else(|| bundle_id.to_owned()))
}

pub(crate) fn project_seeded(harness: &str, home: &Path) -> Result<bool> {
    Ok(open_reader(&database_path())?.query_row(
        "SELECT EXISTS(SELECT 1 FROM project_seed_homes WHERE harness=?1 AND home=?2)",
        params![harness, path_to_blob(home)],
        |row| row.get(0),
    )?)
}

pub(crate) fn finish_project_seed(harness: &str, home: &Path) -> Result<()> {
    let harness = harness.to_owned();
    let home = home.to_owned();
    submit_database_write("finish_project_seed", move |_| {
        let _commit = crate::upgrade::activity_unless_draining("project seed completion")?;
        open(&database_path())?.execute(
            "INSERT OR IGNORE INTO project_seed_homes(harness,home) VALUES(?1,?2)",
            params![harness, path_to_blob(&home)],
        )?;
        Ok(())
    })
}

pub(crate) fn project_discovery_changes(retry: bool) -> Result<Vec<ProjectDiscoveryChange>> {
    let connection = open_reader(&database_path())?;
    let mut statement=connection.prepare("SELECT sequence,session_id,directory,managed_worktree,target_template_id
        FROM project_discovery_changes WHERE sequence>(SELECT sequence FROM project_discovery_progress WHERE singleton=1)
        OR (?1 AND sequence IN (SELECT sequence FROM project_discovery_failures)) ORDER BY sequence LIMIT CASE WHEN ?1 THEN -1 ELSE 128 END")?;
    statement
        .query_map([retry], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .map(|row| {
            let (sequence, session_id, directory, managed, target_id) = row?;
            Ok(ProjectDiscoveryChange {
                sequence,
                session_id,
                directory: blob_to_path(&directory),
                managed_worktree: managed
                    .map(|json| serde_json::from_str(&json))
                    .transpose()?,
                target_id,
            })
        })
        .collect()
}

pub(crate) fn finish_project_discovery(sequence: i64, error: Option<String>) -> Result<()> {
    submit_database_write("finish_project_discovery", move |_| {
        let _commit = crate::upgrade::activity_unless_draining("project discovery progress")?;
        let mut connection = open(&database_path())?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(error) = error {
            tx.execute("INSERT INTO project_discovery_failures(sequence,error) VALUES(?1,?2) ON CONFLICT(sequence) DO UPDATE SET error=excluded.error",params![sequence,error])?;
        } else {
            tx.execute(
                "DELETE FROM project_discovery_failures WHERE sequence=?1",
                [sequence],
            )?;
        }
        tx.execute(
            "UPDATE project_discovery_progress SET sequence=max(sequence,?1) WHERE singleton=1",
            [sequence],
        )?;
        tx.commit()?;
        Ok(())
    })
}

/// Allocate a catalog ID and decide reuse inside the serialized writer.
pub(crate) fn store_directory_project(project: &ProjectBundleSnapshot) -> Result<String> {
    let project = project.clone();
    submit_database_write("store_directory_project", move |_| {
        let _commit = crate::upgrade::activity_unless_draining("directory project catalog")?;
        let connection = open(&database_path())?;
        let key = project.key()?;
        if let Some(id) = connection
            .query_row(
                "SELECT bundle_id FROM project_catalog WHERE project_key=?1",
                [key],
                |row| row.get(0),
            )
            .optional()?
        {
            connection.execute(
                "UPDATE project_catalog SET hidden=0 WHERE bundle_id=?1",
                [&id],
            )?;
            return Ok(id);
        }
        let base = crate::import::setup_style_id(&project.name());
        let mut id = base.clone();
        for suffix in 2_u32.. {
            let used:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM project_catalog WHERE bundle_id=?1 UNION SELECT 1 FROM project_aliases WHERE bundle_id=?1)",[&id],|row|row.get(0))?;
            if !used {
                break;
            }
            id = format!("{base}-{suffix}");
        }
        drop(connection);
        store_catalog_project_at(&database_path(), &id, &project, false)
    })
}

pub(crate) fn bind_session_project(
    session_id: &str,
    bundle_id: &str,
    project: &ProjectBundleSnapshot,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let bundle_id = bundle_id.to_owned();
    let project = project.clone();
    submit_database_write("bind_session_project", move |_| {
        let _commit = crate::upgrade::activity_unless_draining("session project catalog")?;
        let mut connection = open(&database_path())?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Discovery backfills identity; it never changes an accepted shape.
        let updated = tx.execute(
            "UPDATE sessions SET project_json=?2 WHERE session_id=?1 AND project_json IS NULL",
            params![session_id, serde_json::to_string(&project)?],
        )?;
        if updated != 0 {
            tx.execute("INSERT OR IGNORE INTO project_session_aliases(session_id,bundle_id) SELECT session_id,bundle_id FROM session_contexts WHERE session_id=?1",[&session_id])?;
            tx.execute(
                "UPDATE session_contexts SET bundle_id=?2 WHERE session_id=?1",
                params![session_id, bundle_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
}

pub(crate) fn seed_failure(
    harness: &str,
    home: &Path,
    directory: &Path,
    source_file: bool,
    error: Option<String>,
) -> Result<()> {
    let harness = harness.to_owned();
    let home = home.to_owned();
    let directory = directory.to_owned();
    submit_database_write("project_seed_failure", move |_| {
        let _commit = crate::upgrade::activity_unless_draining("project seed retry catalog")?;
        let connection = open(&database_path())?;
        if let Some(error) = error {
            connection.execute("INSERT INTO project_seed_failures(harness,home,directory,error,source_file) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(harness,home,directory) DO UPDATE SET error=excluded.error,source_file=excluded.source_file",params![harness,path_to_blob(&home),path_to_blob(&directory),error,source_file])?;
        } else {
            connection.execute(
                "DELETE FROM project_seed_failures WHERE harness=?1 AND home=?2 AND directory=?3",
                params![harness, path_to_blob(&home), path_to_blob(&directory)],
            )?;
        }
        Ok(())
    })
}

pub(crate) fn seed_failures() -> Result<Vec<(String, PathBuf, PathBuf, bool)>> {
    let connection = open_reader(&database_path())?;
    let mut statement=connection.prepare("SELECT harness,home,directory,source_file FROM project_seed_failures ORDER BY harness,home,directory")?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                blob_to_path(row.get_ref(1)?.as_blob()?),
                blob_to_path(row.get_ref(2)?.as_blob()?),
                row.get::<_, bool>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub(crate) fn discovery_errors() -> Result<Vec<String>> {
    let connection = open_reader(&database_path())?;
    let mut statement=connection.prepare("SELECT error FROM project_discovery_failures UNION SELECT error FROM project_seed_failures ORDER BY error")?;
    statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub(crate) fn saved_project(bundle_id: &str) -> Result<Option<(String, ProjectBundleSnapshot)>> {
    let connection = open_reader(&database_path())?;
    let canonical = canonical_project_id(&connection, bundle_id)?;
    let json:Option<String>=connection.query_row("SELECT snapshot_json FROM project_aliases WHERE bundle_id=?1 UNION ALL SELECT snapshot_json FROM project_catalog WHERE bundle_id=?1 LIMIT 1",[bundle_id],|row|row.get(0)).optional()?;
    json.map(|json| Ok((canonical, serde_json::from_str(&json)?)))
        .transpose()
}

pub(crate) fn hide_catalog_project(bundle_id: &str) -> Result<()> {
    let bundle_id = bundle_id.to_owned();
    submit_database_write("hide_catalog_project", move |_| {
        let _commit = crate::upgrade::activity("remove project catalog entry")?;
        open(&database_path())?.execute(
            "UPDATE project_catalog SET hidden=1 WHERE bundle_id=?1",
            [bundle_id],
        )?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::config::{ProjectBundle, ProjectRepository};
    use mj_core::repository::RepositoryIdentity;

    #[test]
    fn editing_a_project_layout_preserves_old_worker_history_and_checkpoint_ids() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("projects.sqlite");
        let old = ProjectBundleSnapshot {
            bundle: ProjectBundle {
                primary_repo: "app".into(),
                repositories: vec![ProjectRepository {
                    id: "app".into(),
                    github: Some("acme/app".into()),
                    local: None,
                    destination: "app".into(),
                    git_ref: None,
                }],
            },
            identities: BTreeMap::from([(
                "app".into(),
                RepositoryIdentity::Github("acme".into(), "app".into()),
            )]),
            network_sources: BTreeMap::new(),
        };
        let session = super::super::tests::session("old-session", "project");
        super::super::sessions::save_session_to(&path, &session).unwrap();
        store_catalog_project_at(&path, "project", &old, true).unwrap();
        let mut edited = old.clone();
        edited.bundle.repositories[0].destination = "custom-layout".into();
        store_catalog_project_at(&path, "project", &edited, true).unwrap();
        let new_session = super::super::tests::session("new-session", "project");
        super::super::sessions::save_session_to(&path, &new_session).unwrap();
        super::super::prompts::record_prompt_to(
            &path,
            "old-session",
            "project",
            1,
            None,
            "old worker prompt",
        )
        .unwrap();
        super::super::prompts::record_prompt_to(
            &path,
            "new-session",
            "project",
            1,
            None,
            "new worker prompt",
        )
        .unwrap();
        super::super::sessions::save_session_to(&path, &session).unwrap();
        let loaded = super::super::load_state_from(&path).unwrap();
        assert_eq!(loaded.sessions["old-session"].project, Some(old));
        assert_ne!(loaded.sessions["old-session"].bundle_id, "project");
        assert_eq!(loaded.sessions["new-session"].bundle_id, "project");
        let old_history = super::super::prompts::search_prompts_from(
            &path,
            "old-session",
            "project",
            HistoryScope::Project,
            "worker",
        )
        .unwrap();
        let new_history = super::super::prompts::search_prompts_from(
            &path,
            "new-session",
            "project",
            HistoryScope::Project,
            "worker",
        )
        .unwrap();
        assert_eq!(old_history.len(), 1);
        assert_eq!(new_history.len(), 1);
        assert_ne!(old_history[0].text, new_history[0].text);
        assert_eq!(
            read_project_catalog_from(&open(&path).unwrap())
                .unwrap()
                .projects
                .len(),
            1
        );
    }

    #[test]
    fn equivalent_projects_merge_durably_and_preserve_accepted_repository_ids() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("projects.sqlite");
        let make = |id: &str| ProjectBundleSnapshot {
            bundle: ProjectBundle {
                primary_repo: id.into(),
                repositories: vec![ProjectRepository {
                    id: id.into(),
                    github: Some("acme/app".into()),
                    local: None,
                    destination: "app".into(),
                    git_ref: None,
                }],
            },
            identities: BTreeMap::from([(
                id.into(),
                RepositoryIdentity::Github("acme".into(), "app".into()),
            )]),
            network_sources: BTreeMap::new(),
        };
        let primary = make("app");
        let alias = make("legacy-app");
        assert_eq!(
            store_catalog_project_at(&path, "canonical", &primary, true).unwrap(),
            "canonical"
        );
        assert_eq!(
            store_catalog_project_at(&path, "duplicate", &alias, true).unwrap(),
            "canonical"
        );
        let connection = open(&path).unwrap();
        assert_eq!(
            canonical_project_id(&connection, "duplicate").unwrap(),
            "canonical"
        );
        let stored: String = connection
            .query_row(
                "SELECT snapshot_json FROM project_aliases WHERE bundle_id='duplicate'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<ProjectBundleSnapshot>(&stored)
                .unwrap()
                .bundle
                .primary_repo,
            "legacy-app"
        );
        assert_eq!(
            read_project_catalog_from(&connection)
                .unwrap()
                .projects
                .len(),
            1
        );
        drop(connection);
        assert_eq!(
            store_catalog_project_at(&path, "duplicate", &alias, true).unwrap(),
            "canonical"
        );
        assert_eq!(
            read_project_catalog_from(&open(&path).unwrap())
                .unwrap()
                .projects
                .len(),
            1
        );
        let mut edited = alias.clone();
        edited.bundle.repositories[0].destination = "different-layout".into();
        assert_eq!(
            store_catalog_project_at(&path, "duplicate", &edited, true).unwrap(),
            "duplicate"
        );
        assert_eq!(
            canonical_project_id(&open(&path).unwrap(), "duplicate").unwrap(),
            "duplicate"
        );
        assert_eq!(
            read_project_catalog_from(&open(&path).unwrap())
                .unwrap()
                .projects
                .len(),
            2
        );
    }
    #[test]
    fn consolidation_rebinds_history_and_freezes_legacy_session_definition_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("projects.sqlite");
        let make = |id: &str| ProjectBundleSnapshot {
            bundle: ProjectBundle {
                primary_repo: id.into(),
                repositories: vec![ProjectRepository {
                    id: id.into(),
                    github: Some("acme/app".into()),
                    local: None,
                    destination: "app".into(),
                    git_ref: None,
                }],
            },
            identities: BTreeMap::from([(
                id.into(),
                RepositoryIdentity::Github("acme".into(), "app".into()),
            )]),
            network_sources: BTreeMap::new(),
        };
        let canonical = make("app");
        let legacy = make("legacy-app");
        let session = super::super::tests::session("legacy-session", "duplicate");
        super::super::sessions::save_session_to(&path, &session).unwrap();
        super::super::prompts::record_prompt_to(
            &path,
            "legacy-session",
            "duplicate",
            1,
            None,
            "accepted prompt",
        )
        .unwrap();
        store_catalog_project_at(&path, "canonical", &canonical, true).unwrap();
        store_catalog_project_at(&path, "duplicate", &legacy, true).unwrap();
        let loaded =
            super::super::load_state_from(&path).unwrap().sessions["legacy-session"].clone();
        assert_eq!(loaded.bundle_id, "canonical");
        assert_eq!(
            loaded.project.as_ref().unwrap().bundle.primary_repo,
            "legacy-app"
        );
        // A worker still presents the old ID after a merge.
        super::super::prompts::record_prompt_to(
            &path,
            "legacy-session",
            "duplicate",
            2,
            None,
            "late prompt",
        )
        .unwrap();
        super::super::sessions::save_session_to(&path, &session).unwrap();
        let history = super::super::prompts::search_prompts_from(
            &path,
            "other-session",
            "duplicate",
            HistoryScope::Project,
            "prompt",
        )
        .unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(
            super::super::load_state_from(&path).unwrap().sessions["legacy-session"].project,
            Some(legacy)
        );
    }
}

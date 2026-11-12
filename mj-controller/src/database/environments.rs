//! Physical ownership outlives logical sessions and their parent relationships.
use super::*;
use mj_core::state::{SessionBuildCache, TargetRuntimeSettings};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvironmentPhase {
    Active,
    Retiring,
    Reclaimed,
}

impl EnvironmentPhase {
    fn from_stored(value: &str) -> Result<Self> {
        match value {
            "active" => Ok(Self::Active),
            "retiring" => Ok(Self::Retiring),
            "reclaimed" => Ok(Self::Reclaimed),
            _ => bail!("unknown environment phase {value:?}"),
        }
    }
}

/// Resource metadata only: no prompt history, agent state or logical lifecycle.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentPlacement {
    #[serde(skip)]
    pub target: Option<TargetLocator>,
    pub target_template_id: String,
    pub runtime: Option<TargetRuntimeSettings>,
    pub managed_worktree: Option<ManagedWorktree>,
    #[serde(with = "optional_path")]
    pub project_directory: Option<PathBuf>,
    #[serde(with = "optional_path")]
    pub container_workspace: Option<PathBuf>,
    pub build_cache: Option<SessionBuildCache>,
    pub repositories: Option<Vec<PathBuf>>,
    pub bundle_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_destination: Option<EnvironmentPreparedDestination>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentPreparedDestination {
    pub operation_id: String,
    pub destination: mj_core::state::PreparedMoveDestination,
}

impl EnvironmentPlacement {
    pub(crate) fn from_session(session: &SessionRecord) -> Self {
        Self {
            target: session.target.clone().map(|v| *v),
            target_template_id: session.target_template_id.clone(),
            runtime: session.target_runtime.as_deref().cloned(),
            managed_worktree: session.managed_worktree.clone().map(|v| *v),
            project_directory: session.project_directory.clone(),
            container_workspace: session.container_workspace.clone(),
            build_cache: session.build_cache.clone(),
            repositories: session.project.as_ref().map(|project| {
                project
                    .bundle
                    .repositories
                    .iter()
                    .map(|repository| repository.destination.clone())
                    .collect()
            }),
            bundle_id: session.bundle_id.clone(),
            prepared_destination: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EnvironmentRecord {
    pub id: String,
    pub owner_session_id: String,
    pub phase: EnvironmentPhase,
    pub placement: EnvironmentPlacement,
}

#[derive(Clone, Debug)]
pub struct EnvironmentWorker {
    lease_id: String,
    pub session_id: String,
    pub target: TargetLocator,
    pub target_template_id: String,
    pub runtime: Option<TargetRuntimeSettings>,
}

pub(super) fn read_environment(
    connection: &Connection,
    id: &str,
) -> Result<Option<EnvironmentRecord>> {
    let row = connection.query_row("SELECT owner_session_id,phase,placement_json,target_lease_id FROM environments WHERE environment_id=?1",[id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,Option<String>>(3)?))).optional()?;
    row.map(|(owner_session_id,phase,json,lease)| {
        let mut placement: EnvironmentPlacement = serde_json::from_str(&json).context("read environment placement")?;
        placement.target = lease.map(|lease| connection.query_row("SELECT kind,host,resource_id,address,workspace,worker_id,workspace_storage,borrowed_from FROM environment_workers WHERE lease_id=?1",[lease],|row|super::state_io::decode_target(row,0))).transpose()?;
        Ok(EnvironmentRecord {id:id.to_owned(),owner_session_id,phase:EnvironmentPhase::from_stored(&phase)?,placement})
    }).transpose()
}

pub fn session_environment(session_id: &str) -> Result<Option<EnvironmentRecord>> {
    let connection = open_reader(&database_path())?;
    let id: Option<String> = connection
        .query_row(
            "SELECT environment_id FROM environment_bindings WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?;
    id.map(|id| read_environment(&connection, &id))
        .transpose()
        .map(Option::flatten)
}

pub fn environment_workers(environment_id: &str) -> Result<Vec<EnvironmentWorker>> {
    let connection = open_reader(&database_path())?;
    read_environment_workers(&connection, environment_id)
}

fn read_environment_workers(
    connection: &Connection,
    environment_id: &str,
) -> Result<Vec<EnvironmentWorker>> {
    let mut query = connection.prepare("SELECT worker_session_id,kind,host,resource_id,address,workspace,worker_id,workspace_storage,borrowed_from,target_template_id,runtime_json,lease_id FROM environment_workers WHERE environment_id=?1 ORDER BY worker_session_id,lease_id")?;
    let rows = query.query_map([environment_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            super::state_io::decode_target(row, 1)?,
            row.get::<_, String>(9)?,
            row.get::<_, Option<String>>(10)?,
            row.get::<_, String>(11)?,
        ))
    })?;
    rows.map(|row| {
        let (session_id, target, target_template_id, runtime, lease_id) = row?;
        Ok(EnvironmentWorker {
            lease_id,
            session_id,
            target,
            target_template_id,
            runtime: runtime
                .map(|json| serde_json::from_str(&json))
                .transpose()
                .context("read environment worker access")?,
        })
    })
    .collect()
}

fn store_worker(
    transaction: &Transaction<'_>,
    environment_id: &str,
    session_id: &str,
    target_template_id: &str,
    runtime: Option<&TargetRuntimeSettings>,
    target: &TargetLocator,
) -> Result<String> {
    let existing = read_environment_workers(transaction, environment_id)?
        .into_iter()
        .find(|worker| worker.session_id == session_id && worker.target.same_resource(target));
    let lease = existing
        .map(|worker| worker.lease_id)
        .unwrap_or(mj_core::state::new_session_id()?);
    let fields = super::state_io::stored_target_values(target)?;
    transaction.execute("INSERT INTO environment_workers(lease_id,environment_id,worker_session_id,target_template_id,runtime_json,kind,host,resource_id,address,workspace,worker_id,workspace_storage,borrowed_from) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13) ON CONFLICT(lease_id) DO UPDATE SET address=excluded.address,workspace_storage=excluded.workspace_storage,runtime_json=coalesce(environment_workers.runtime_json,excluded.runtime_json)",params![lease,environment_id,session_id,target_template_id,runtime.map(serde_json::to_string).transpose()?,fields.kind,fields.host,fields.resource,fields.address,fields.workspace,fields.worker_id,fields.workspace_storage,fields.borrowed_from])?;
    if read_environment(transaction, environment_id)?
        .is_some_and(|environment| environment.owner_session_id == session_id)
    {
        transaction.execute(
            "UPDATE environments SET target_lease_id=?2 WHERE environment_id=?1",
            params![environment_id, lease],
        )?;
    }
    Ok(lease)
}

/// Called in the same transaction that publishes a placement. A binding is
/// presentation; the environment and its worker leases never cascade with it.
pub(super) fn capture_session_environment(
    transaction: &Transaction<'_>,
    session: &SessionRecord,
    parent: Option<&str>,
) -> Result<()> {
    let bound: Option<String> = transaction
        .query_row(
            "SELECT environment_id FROM environment_bindings WHERE session_id=?1",
            [&session.id],
            |row| row.get(0),
        )
        .optional()?;
    let id = if let Some(mut id) = bound {
        let parent: Option<String> = transaction
            .query_row(
                "SELECT parent_session_id FROM subagent_sessions WHERE child_session_id=?1",
                [&session.id],
                |row| row.get(0),
            )
            .optional()?;
        if session.target.is_some()
            && let Some(parent) = parent
        {
            let current: String = transaction.query_row(
                "SELECT environment_id FROM environment_bindings WHERE session_id=?1",
                [parent],
                |row| row.get(0),
            )?;
            if current != id {
                let current_owner = read_environment(transaction, &current)?
                    .context("parent environment disappeared")?;
                ensure!(
                    current_owner.phase == EnvironmentPhase::Active,
                    "parent environment is retiring; worker admission is closed"
                );
                transaction.execute(
                    "UPDATE environment_bindings SET environment_id=?2 WHERE session_id=?1",
                    params![session.id, current],
                )?;
                id = current;
            }
        }
        id
    } else {
        let parent = match parent {
            Some(parent) => Some(parent.to_owned()),
            None => transaction
                .query_row(
                    "SELECT parent_session_id FROM subagent_sessions WHERE child_session_id=?1",
                    [&session.id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?,
        };
        let inherited: Option<String> = parent
            .as_ref()
            .map(|parent| {
                transaction
                    .query_row(
                        "SELECT environment_id FROM environment_bindings WHERE session_id=?1",
                        [parent],
                        |row| row.get(0),
                    )
                    .optional()
            })
            .transpose()?
            .flatten();
        let id = match inherited {
            Some(id) => id,
            None => {
                ensure!(
                    parent.is_none(),
                    "parent environment must be registered before its worker"
                );
                let id = mj_core::state::new_session_id()?;
                transaction.execute("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES (?1,?2,'active',?3)",
                    params![id, session.id, serde_json::to_string(&EnvironmentPlacement::from_session(session))?])?;
                id
            }
        };
        ensure!(
            read_environment(transaction, &id)?
                .is_some_and(|environment| environment.phase == EnvironmentPhase::Active),
            "environment is retiring; child registration is closed"
        );
        transaction.execute(
            "INSERT INTO environment_bindings(session_id,environment_id) VALUES (?1,?2)",
            params![session.id, id],
        )?;
        id
    };
    let mut environment =
        read_environment(transaction, &id)?.context("session environment disappeared")?;
    let mut id = id;
    let next = EnvironmentPlacement::from_session(session);
    let storage_changes = match (
        &environment.placement.managed_worktree,
        &next.managed_worktree,
    ) {
        (Some(before), Some(after)) => {
            before.worktree_root != after.worktree_root
                || !before.target.same_location(&after.target)
        }
        (Some(_), None) => true,
        (None, Some(_)) => environment.placement.target.is_some(),
        (None, None) => false,
    };
    let new_target = session.target.as_ref().is_some_and(|next| {
        environment
            .placement
            .target
            .as_ref()
            .is_some_and(|target| !target.same_resource(next))
    });
    let resume_reclaimed = environment.phase == EnvironmentPhase::Reclaimed
        && session.state == SessionState::Provisioning;
    if environment.owner_session_id == session.id
        && (resume_reclaimed || storage_changes || new_target)
    {
        // This transaction publishes a new physical placement. Keep the old
        // owner and leases for Move retirement and interrupted cleanup.
        id = mj_core::state::new_session_id()?;
        transaction.execute("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES (?1,?2,'active',?3)",
            params![id, session.id, serde_json::to_string(&EnvironmentPlacement::from_session(session))?])?;
        transaction.execute(
            "UPDATE environment_bindings SET environment_id=?2 WHERE session_id=?1",
            params![session.id, id],
        )?;
        environment = read_environment(transaction, &id)?.context("new environment disappeared")?;
    }
    if environment.phase == EnvironmentPhase::Reclaimed {
        return Ok(());
    }
    if environment.owner_session_id == session.id
        && (session.target.is_some() || environment.placement.target.is_none())
    {
        transaction.execute(
            "UPDATE environments SET placement_json=?2 WHERE environment_id=?1",
            params![
                id,
                serde_json::to_string(&EnvironmentPlacement::from_session(session))?
            ],
        )?;
    }
    if let Some(target) = &session.target {
        let existing = read_environment_workers(transaction, &id)?
            .iter()
            .any(|worker| worker.session_id == session.id && worker.target.same_resource(target));
        ensure!(
            existing || environment.phase == EnvironmentPhase::Active,
            "environment is retiring; new worker admission is closed"
        );
        store_worker(
            transaction,
            &id,
            &session.id,
            &session.target_template_id,
            session.target_runtime.as_deref(),
            target,
        )?;
    }

    Ok(())
}

/// Closing admission and selecting the writers are one serialized decision.
pub fn begin_environment_retirement(
    environment_id: &str,
) -> Result<(EnvironmentRecord, Vec<EnvironmentWorker>)> {
    let id = environment_id.to_owned();
    submit_database_write("retire environment", move |connection| {
        let transaction = connection.transaction()?;
        let mut environment =
            read_environment(&transaction, &id)?.context("unknown environment")?;
        if environment.phase == EnvironmentPhase::Reclaimed {
            return Ok((environment, Vec::new()));
        }
        transaction.execute(
            "UPDATE environments SET phase='retiring' WHERE environment_id=?1",
            [&id],
        )?;
        let workers = read_environment_workers(&transaction, &id)?;
        transaction.commit()?;
        environment.phase = EnvironmentPhase::Retiring;
        Ok((environment, workers))
    })
}

pub fn finish_environment_retirement(environment_id: &str) -> Result<()> {
    let id = environment_id.to_owned();
    submit_database_write("finish environment retirement", move |connection| {
        let transaction = connection.transaction()?;
        let changed = transaction.execute("UPDATE environments SET phase='reclaimed' WHERE environment_id=?1 AND phase='retiring'", [&id])?;
        ensure!(
            changed == 1,
            "environment retirement no longer owns this environment"
        );
        // Keep reclaimed descriptors as tombstones while a logical record can
        // still retry settlement. They are not admitted worker leases.
        transaction.execute(
            "DELETE FROM environment_reports WHERE environment_id=?1",
            [&id],
        )?;
        transaction.commit()?;
        Ok(())
    })
}

/// A runtime can be replaced while its workspace remains owned by this same
/// environment. Only the owner, after draining every writer and removing the
/// old runtime, may release those leases and reopen launch admission.
pub fn finish_environment_runtime_removal(environment_id: &str) -> Result<()> {
    let id = environment_id.to_owned();
    submit_database_write("finish environment runtime removal", move |connection| {
        let transaction = connection.transaction()?;
        let mut environment =
            read_environment(&transaction, &id)?.context("environment disappeared")?;
        ensure!(
            environment.phase == EnvironmentPhase::Retiring,
            "environment no longer owns runtime removal"
        );
        environment.placement.target = None;
        transaction.execute(
            "DELETE FROM environment_workers WHERE environment_id=?1",
            [&id],
        )?;
        transaction.execute(
            "UPDATE environments SET phase='active',placement_json=?2 WHERE environment_id=?1",
            params![id, serde_json::to_string(&environment.placement)?],
        )?;
        transaction.execute("UPDATE sessions SET state='error',last_error='The shared environment was stopped for recovery.' WHERE session_id IN (SELECT session_id FROM environment_bindings WHERE environment_id=?1) AND session_id<>?2 AND state IN ('running','disconnected')",params![id,environment.owner_session_id])?;
        transaction.commit()?;
        Ok(())
    })
}

/// Migration uses the existing target decoder, including targets whose harness
/// is no longer supported. Physical ownership must not depend on that parser.
pub(super) fn backfill_environments(transaction: &Transaction<'_>) -> Result<()> {
    let targets = super::state_io::read_targets(transaction, None)?;
    let parents: BTreeMap<String, String> = transaction
        .prepare("SELECT child_session_id,parent_session_id FROM subagent_sessions")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut records: BTreeMap<String, EnvironmentPlacement> = BTreeMap::new();
    let mut statement = transaction.prepare("SELECT s.session_id,s.target_template_id,s.target_runtime_json,s.managed_worktree,s.project_directory,s.container_workspace,s.build_cache_json,s.project_json,c.bundle_id FROM sessions s JOIN session_contexts c USING(session_id)")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get_ref(4)?.blob_or_null()?.map(blob_to_path),
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, String>(8)?,
        ))
    })?;
    for row in rows {
        let (
            id,
            target_template_id,
            runtime,
            worktree,
            project_directory,
            container_workspace,
            cache,
            project,
            bundle_id,
        ) = row?;
        records.insert(
            id.clone(),
            EnvironmentPlacement {
                target: targets.get(&id).cloned(),
                target_template_id,
                runtime: runtime
                    .map(|json| serde_json::from_str(&json))
                    .transpose()?,
                managed_worktree: worktree
                    .map(|json| serde_json::from_str(&json))
                    .transpose()?,
                project_directory,
                container_workspace: container_workspace.map(PathBuf::from),
                build_cache: cache.map(|json| serde_json::from_str(&json)).transpose()?,
                repositories: project
                    .map(|json| serde_json::from_str::<serde_json::Value>(&json))
                    .transpose()?
                    .as_ref()
                    .and_then(project_repositories),
                bundle_id,
                prepared_destination: None,
            },
        );
    }
    let mut owners = BTreeMap::new();
    for id in records.keys() {
        let mut owner = id.as_str();
        let mut visited = BTreeSet::new();
        while let Some(parent) = parents.get(owner) {
            ensure!(
                visited.insert(owner),
                "cyclic environment ownership in stored sub-agents"
            );
            owner = parent;
        }
        ensure!(
            records.contains_key(owner),
            "environment owner {owner} has no recorded placement"
        );
        owners.insert(id.clone(), owner.to_owned());
    }
    let mut environment_ids = BTreeMap::new();
    for owner in owners.values().collect::<BTreeSet<_>>() {
        let id = mj_core::state::new_session_id()?;
        transaction.execute("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES (?1,?2,'active',?3)", params![id,owner,serde_json::to_string(&records[owner])?])?;
        environment_ids.insert(owner, id);
    }
    for (worker, placement) in records {
        let id = &environment_ids[&owners[&worker]];
        transaction.execute(
            "INSERT INTO environment_bindings(session_id,environment_id) VALUES (?1,?2)",
            params![worker, id],
        )?;
        if let Some(target) = placement.target {
            store_worker(
                transaction,
                id,
                &worker,
                &placement.target_template_id,
                placement.runtime.as_ref(),
                &target,
            )?;
        }
    }
    transaction.execute("INSERT OR IGNORE INTO environment_reports SELECT b.environment_id,h.child_session_id,h.report_dir FROM subagent_handbacks h JOIN environment_bindings b ON b.session_id=h.child_session_id WHERE h.report_dir IS NOT NULL",[])?;
    Ok(())
}

/// All physical obligations for a worker, even after its session was deleted.
pub fn worker_environments(session_id: &str) -> Result<Vec<EnvironmentRecord>> {
    let connection = open_reader(&database_path())?;
    let mut query = connection.prepare("SELECT environment_id FROM environments WHERE owner_session_id=?1 OR environment_id IN (SELECT environment_id FROM environment_workers WHERE worker_session_id=?1) ORDER BY rowid")?;
    let ids = query
        .query_map([session_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ids.into_iter()
        .map(|id| read_environment(&connection, &id)?.context("environment disappeared"))
        .collect()
}

/// Launch admission and retirement use the same serialized writer. The caller
/// holds its WorkerPermit until launch finishes; retirement drains that permit.
pub fn admit_environment_worker(session_id: &str, target: &TargetLocator) -> Result<()> {
    let session_id = session_id.to_owned();
    let target = target.clone();
    submit_database_write("admit environment worker", move |connection| {
        let id: Option<String> = connection.query_row("SELECT b.environment_id FROM environment_bindings b JOIN environments e USING(environment_id) WHERE b.session_id=?1 AND e.phase='active'",[&session_id],|row|row.get(0)).optional()?;
        let id = id.context(
            "environment is retiring or worker has no committed placement; worker launch refused",
        )?;
        ensure!(
            read_environment_workers(connection, &id)?
                .iter()
                .any(|worker| worker.session_id == session_id && worker.target == target),
            "worker has no admitted environment placement"
        );
        Ok(())
    })
}

pub fn checkout_environments(worktree: &ManagedWorktree) -> Result<Vec<EnvironmentRecord>> {
    let connection = open_reader(&database_path())?;
    let mut query = connection.prepare("SELECT environment_id FROM environments ORDER BY rowid")?;
    let ids = query
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut environments = Vec::new();
    for id in ids {
        let environment = read_environment(&connection, &id)?.context("environment disappeared")?;
        if environment.placement.managed_worktree.as_ref() == Some(worktree) {
            environments.push(environment);
        }
    }
    Ok(environments)
}

pub fn environment_reports(environment_id: &str) -> Result<Vec<(String, String)>> {
    let connection = open_reader(&database_path())?;
    let mut query = connection.prepare("SELECT child_session_id,report_dir FROM environment_reports WHERE environment_id=?1 ORDER BY child_session_id,report_dir")?;
    Ok(query
        .query_map([environment_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

// Keep the existing database path encoding instead of forcing user paths into
// JSON strings. On Unix this preserves arbitrary bytes; Windows uses UTF-16.
mod optional_path {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        path: &Option<PathBuf>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        path.as_deref().map(path_to_blob).serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Option<PathBuf>, D::Error> {
        Ok(Option::<Vec<u8>>::deserialize(deserializer)?.map(|bytes| blob_to_path(&bytes)))
    }
}

/// Close registration and select the logical children in one writer decision.
/// Physical evidence is untouched; a crash still leaves a retryable record.
pub fn begin_session_destroy(session_id: &str) -> Result<Vec<String>> {
    let id = session_id.to_owned();
    submit_database_write("begin session destroy", move |connection| {
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE sessions SET state='destroying' WHERE session_id=?1",
            [&id],
        )?;
        transaction.execute(
            "UPDATE environments SET phase='retiring' WHERE owner_session_id=?1 AND phase='active'",
            [&id],
        )?;
        let children = transaction.prepare("SELECT child_session_id FROM subagent_sessions WHERE parent_session_id=?1 ORDER BY child_session_id")?.query_map([&id],|row|row.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        transaction.commit()?;
        Ok(children)
    })
}

/// Physical fields from an old saved SessionRecord. Harness parsing must not
/// erase an independently persisted Move source during upgrade.
fn historical_placement(value: &serde_json::Value) -> Result<EnvironmentPlacement> {
    let mut metadata = serde_json::Map::new();
    for key in [
        "target_template_id",
        "managed_worktree",
        "build_cache",
        "bundle_id",
    ] {
        metadata.insert(
            key.to_owned(),
            value.get(key).cloned().unwrap_or_else(|| {
                if matches!(key, "target_template_id" | "bundle_id") {
                    serde_json::Value::String(String::new())
                } else {
                    serde_json::Value::Null
                }
            }),
        );
    }
    metadata.insert(
        "repositories".into(),
        serde_json::to_value(value.get("project").and_then(project_repositories))?,
    );
    metadata.insert(
        "runtime".into(),
        value
            .get("target_runtime")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    );
    for key in ["project_directory", "container_workspace"] {
        let path = value.get(key).cloned().unwrap_or(serde_json::Value::Null);
        let path: Option<PathBuf> = serde_json::from_value(path)?;
        metadata.insert(
            key.into(),
            serde_json::to_value(path.as_deref().map(path_to_blob))?,
        );
    }
    let mut placement: EnvironmentPlacement =
        serde_json::from_value(serde_json::Value::Object(metadata))?;
    placement.target = value
        .get("target")
        .cloned()
        .map(serde_json::from_value::<Option<TargetLocator>>)
        .transpose()?
        .flatten();
    Ok(placement)
}

fn register_historical_environment(
    transaction: &Transaction<'_>,
    owner: &str,
    placement: EnvironmentPlacement,
) -> Result<String> {
    let ids = transaction
        .prepare(
            "SELECT environment_id FROM environments WHERE owner_session_id=?1 ORDER BY rowid",
        )?
        .query_map([owner], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        let candidate =
            read_environment(transaction, &id)?.context("historical environment disappeared")?;
        let same_target = match (&candidate.placement.target, &placement.target) {
            (Some(left), Some(right)) => left.same_resource(right),
            (None, None) => true,
            _ => false,
        };
        let same_checkout = match (
            &candidate.placement.managed_worktree,
            &placement.managed_worktree,
        ) {
            (Some(left), Some(right)) => {
                left.worktree_root == right.worktree_root
                    && left.target.same_location(&right.target)
            }
            (None, None) => true,
            _ => false,
        };
        if same_target && same_checkout {
            return Ok(id);
        }
    }
    let id = mj_core::state::new_session_id()?;
    transaction.execute("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES (?1,?2,'active',?3)",params![id,owner,serde_json::to_string(&placement)?])?;
    if let Some(target) = &placement.target {
        store_worker(
            transaction,
            &id,
            owner,
            &placement.target_template_id,
            placement.runtime.as_ref(),
            target,
        )?;
    }
    Ok(id)
}

pub(super) fn backfill_move_environments(transaction: &Transaction<'_>) -> Result<()> {
    let retained = transaction
        .prepare("SELECT operation_id,session_id,source_json FROM retained_move_sources")?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (operation, owner, json) in retained {
        let value: serde_json::Value = serde_json::from_str(&json)?;
        let id =
            register_historical_environment(transaction, &owner, historical_placement(&value)?)?;
        transaction.execute(
            "UPDATE retained_move_sources SET environment_id=?2 WHERE operation_id=?1",
            params![operation, id],
        )?;
    }
    let moves = transaction
        .prepare("SELECT session_id,operation_json FROM session_moves")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (owner, json) in moves {
        let value: serde_json::Value = serde_json::from_str(&json)?;
        if let Some(prepared) = value
            .get("prepared_destination")
            .filter(|value| !value.is_null())
        {
            let destination: mj_core::state::PreparedMoveDestination =
                serde_json::from_value(prepared.clone())?;
            if let mj_core::state::PreparedDestinationState::Adopted { target } = &destination.state
            {
                register_historical_environment(
                    transaction,
                    &owner,
                    EnvironmentPlacement {
                        target: Some(target.clone()),
                        runtime: Some(destination.runtime.clone()),
                        ..Default::default()
                    },
                )?;
            } else if destination.owns_resource() {
                let placement = EnvironmentPlacement {
                    prepared_destination: Some(EnvironmentPreparedDestination {
                        operation_id: value
                            .get("operation_id")
                            .and_then(|value| value.as_str())
                            .context("prepared Move has no operation ID")?
                            .to_owned(),
                        destination,
                    }),
                    ..Default::default()
                };
                let id = mj_core::state::new_session_id()?;
                transaction.execute("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES (?1,?2,'active',?3)",params![id,owner,serde_json::to_string(&placement)?])?;
            }
        }
        let source = value.pointer("/workspace_transfer/source").or_else(|| {
            value
                .get("recovery_session")
                .filter(|value| !value.is_null())
        });
        let id = if let Some(source) = source {
            register_historical_environment(transaction, &owner, historical_placement(source)?)?
        } else {
            transaction.query_row(
                "SELECT environment_id FROM environment_bindings WHERE session_id=?1",
                [&owner],
                |row| row.get::<_, String>(0),
            )?
        };
        transaction.execute(
            "UPDATE session_moves SET source_environment_id=?2 WHERE session_id=?1",
            params![owner, id],
        )?;
    }
    Ok(())
}

/// A stopped owned checkout is reserved before Resume materializes or mutates
/// it. Its old reclaimed environment stays a tombstone; the new reservation
/// owns construction and can be reclaimed even if no worker ever starts.
pub(crate) fn reserve_owned_checkout(session: &SessionRecord) -> Result<EnvironmentRecord> {
    let session = session.clone();
    submit_database_write("reserve resumed checkout", move |connection| {
        let transaction = connection.transaction()?;
        let current_id: String = transaction.query_row(
            "SELECT environment_id FROM environment_bindings WHERE session_id=?1",
            [&session.id],
            |row| row.get(0),
        )?;
        let current =
            read_environment(&transaction, &current_id)?.context("checkout owner disappeared")?;
        ensure!(
            current.owner_session_id == session.id,
            "cannot reserve a borrowed checkout"
        );
        ensure!(
            current.phase != EnvironmentPhase::Retiring,
            "shared workspace cleanup is incomplete; retry its cleanup before reusing it"
        );
        let id = if current.phase == EnvironmentPhase::Reclaimed {
            let id = mj_core::state::new_session_id()?;
            let mut placement = EnvironmentPlacement::from_session(&session);
            placement.target = None;
            transaction.execute("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES (?1,?2,'active',?3)",params![id,session.id,serde_json::to_string(&placement)?])?;
            transaction.execute(
                "UPDATE environment_bindings SET environment_id=?2 WHERE session_id=?1",
                params![session.id, id],
            )?;
            id
        } else {
            current_id
        };
        let environment =
            read_environment(&transaction, &id)?.context("checkout reservation disappeared")?;
        transaction.commit()?;
        Ok(environment)
    })
}

fn project_repositories(value: &serde_json::Value) -> Option<Vec<PathBuf>> {
    value
        .pointer("/bundle/repositories")?
        .as_array()?
        .iter()
        .map(|repository| {
            let path = PathBuf::from(repository.get("destination")?.as_str()?);
            mj_core::config::validate_relative_destination(&path).ok()?;
            Some(path)
        })
        .collect()
}

pub(crate) fn prepared_move_environment(operation_id: &str) -> Result<Option<EnvironmentRecord>> {
    let connection = open_reader(&database_path())?;
    let id: Option<String> = connection.query_row("SELECT environment_id FROM environments WHERE json_extract(placement_json,'$.prepared_destination.operation_id')=?1",[operation_id],|row|row.get(0)).optional()?;
    id.map(|id| read_environment(&connection, &id))
        .transpose()
        .map(Option::flatten)
}

/// Legacy prepared destinations reserve cleanup independently of the Move row.
/// Initialize once; only the physical owner advances its cleanup identity.
pub(super) fn record_prepared_move_environment(
    connection: &Connection,
    operation: &mj_core::state::MoveOperation,
) -> Result<()> {
    if let Some(mj_core::state::PreparedMoveDestination {
        state: mj_core::state::PreparedDestinationState::Adopted { target },
        ..
    }) = operation.prepared_destination.as_ref()
    {
        let id: String = connection.query_row(
            "SELECT environment_id FROM environment_bindings WHERE session_id=?1",
            [&operation.selection.session_id],
            |row| row.get(0),
        )?;
        let owner =
            read_environment(connection, &id)?.context("adopted destination owner disappeared")?;
        ensure!(
            owner
                .placement
                .target
                .as_ref()
                .is_some_and(|stored| stored.same_resource(target)),
            "adopted destination has no committed worker placement"
        );
        // Adoption transfers cleanup to the worker environment in this same
        // publication transaction; the preparation must never terminate it.
        connection.execute("UPDATE environments SET phase='reclaimed' WHERE json_extract(placement_json,'$.prepared_destination.operation_id')=?1", [&operation.operation_id])?;
        return Ok(());
    }
    let Some(destination) = operation
        .prepared_destination
        .as_ref()
        .filter(|destination| {
            destination.owns_resource()
                && !matches!(
                    destination.state,
                    mj_core::state::PreparedDestinationState::Adopted { .. }
                )
        })
    else {
        return Ok(());
    };
    let exists: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM environments WHERE json_extract(placement_json,'$.prepared_destination.operation_id')=?1)",[&operation.operation_id],|row|row.get(0))?;
    if !exists {
        let id = mj_core::state::new_session_id()?;
        let placement = EnvironmentPlacement {
            prepared_destination: Some(EnvironmentPreparedDestination {
                operation_id: operation.operation_id.clone(),
                destination: destination.clone(),
            }),
            ..Default::default()
        };
        connection.execute("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES (?1,?2,'active',?3)",params![id,operation.selection.session_id,serde_json::to_string(&placement)?])?;
    }
    Ok(())
}

pub(crate) fn record_prepared_environment_instance(
    environment_id: &str,
    instance_id: &str,
) -> Result<()> {
    let environment_id = environment_id.to_owned();
    let instance_id = instance_id.to_owned();
    submit_database_write("record prepared environment instance", move |connection| {
        let transaction = connection.transaction()?;
        let mut environment = read_environment(&transaction, &environment_id)?
            .context("prepared environment disappeared")?;
        ensure!(
            environment.phase == EnvironmentPhase::Retiring,
            "prepared environment is no longer retiring"
        );
        let prepared = environment
            .placement
            .prepared_destination
            .as_mut()
            .context("environment is not a prepared destination")?;
        prepared.destination.state = mj_core::state::PreparedDestinationState::CleanupPending {
            instance_id: Some(instance_id),
        };
        transaction.execute(
            "UPDATE environments SET placement_json=?2 WHERE environment_id=?1",
            params![
                environment_id,
                serde_json::to_string(&environment.placement)?
            ],
        )?;
        transaction.commit()?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hard-won: deleting an unreachable child erased the last evidence of a live shared-workspace writer.
    #[test]
    fn migration_and_logical_deletion_preserve_physical_workers_including_unknown_harnesses() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ownership.sqlite3");
        let mut connection = super::super::schema::open_writer(&path).unwrap();
        let mut parent = crate::controller::test_support::checkpoint_test_session(
            "22222222222222222222222222222222",
        );
        parent.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: directory.path().join(&parent.id),
        }));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let path = directory
                .path()
                .join(std::ffi::OsStr::from_bytes(b"physical-\xff"));
            parent.project_directory = Some(path.clone());
            parent.target = Some(Box::new(TargetLocator::LocalBare {
                worker_root: path.join(&parent.id),
            }));
        }
        let mut child = parent.clone();
        child.id = "33333333333333333333333333333333".into();
        child.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: directory.path().join(&child.id),
        }));
        let transaction = connection.transaction().unwrap();
        super::super::state_io::insert_session(&transaction, &parent).unwrap();
        super::super::state_io::insert_session_with_parent(&transaction, &child, Some(&parent.id))
            .unwrap();
        transaction.execute("INSERT INTO subagent_sessions(child_session_id,parent_session_id,request_key,record_json) VALUES (?1,?2,'fixture','{}')",params![child.id,parent.id]).unwrap();
        transaction.commit().unwrap();
        let mut source = child.clone();
        source.id = parent.id.clone();
        source.project_directory = None;
        source.target = Some(Box::new(TargetLocator::LocalBare {
            worker_root: directory.path().join("old-source"),
        }));
        let mut source_json = serde_json::to_value(&source).unwrap();
        source_json["harness_kind"] = "removed-harness".into();
        let mut runtime = TargetRuntimeSettings::from(&mj_core::config::TargetTemplate::LocalBare);
        runtime.kind = "aws-ec2".into();
        runtime.connection = mj_core::state::TargetConnection::Aws {
            profile: "fixture".into(),
            region: "us-east-1".into(),
            ssh_user: "ubuntu".into(),
            identity_file: None,
            ssh_args: Vec::new(),
        };
        let prepared = mj_core::state::PreparedMoveDestination {
            launch_args: vec!["--client-token".into(), "fixture-token".into()],
            runtime,
            state: mj_core::state::PreparedDestinationState::Created {
                instance_id: "i-fixture".into(),
            },
        };
        let operation = serde_json::json!({"operation_id":"old-move", "workspace_transfer":{"source":source_json}, "prepared_destination": prepared});
        connection.execute("INSERT INTO session_moves(session_id,operation_id,operation_json) VALUES (?1,'old-move',?2)",params![parent.id,operation.to_string()]).unwrap();
        connection.execute("INSERT INTO retained_move_sources(operation_id,session_id,source_json,exclusions_json,created_at) VALUES ('old-move',?1,?2,'[]','now')",params![parent.id,serde_json::to_string(&source_json).unwrap()]).unwrap();
        connection.execute_batch("PRAGMA ignore_check_constraints=ON; UPDATE sessions SET harness_kind='removed-harness' WHERE session_id='33333333333333333333333333333333';
            PRAGMA ignore_check_constraints=OFF; ALTER TABLE session_moves DROP COLUMN source_environment_id; ALTER TABLE retained_move_sources DROP COLUMN environment_id; DROP TRIGGER environment_report_registered; DROP TRIGGER environment_report_updated; DROP TRIGGER reclaimed_environment_owner_deleted; DROP TRIGGER reclaimed_environment_binding_deleted; DROP TABLE environment_reports; DROP TABLE environment_bindings; DROP TABLE environment_workers; DROP TABLE environments;
            DELETE FROM schema_migrations WHERE version>=122;
            UPDATE schema_compatibility SET minimum_compatible_version=121;
            PRAGMA user_version=121;
            CREATE TRIGGER interrupt_ownership_migration BEFORE INSERT ON schema_migrations WHEN NEW.version=122 BEGIN SELECT RAISE(ABORT,'fixture interruption'); END;").unwrap();
        assert!(super::super::schema::migrate_schema(&connection).is_err());
        assert_eq!(
            connection
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            121
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name='environments'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT minimum_compatible_version FROM schema_compatibility",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            121
        );
        connection
            .execute_batch("DROP TRIGGER interrupt_ownership_migration")
            .unwrap();
        super::super::schema::migrate_schema(&connection).unwrap();
        let environment_id: String = connection
            .query_row(
                "SELECT environment_id FROM environment_bindings WHERE session_id=?1",
                [&parent.id],
                |row| row.get(0),
            )
            .unwrap();
        let source_environment: String = connection
            .query_row(
                "SELECT source_environment_id FROM session_moves WHERE operation_id='old-move'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let retained_environment: String = connection
            .query_row(
                "SELECT environment_id FROM retained_move_sources WHERE operation_id='old-move'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(environment_id, source_environment);
        assert_eq!(source_environment, retained_environment);
        assert_eq!(
            read_environment(&connection, &source_environment)
                .unwrap()
                .unwrap()
                .placement
                .target,
            source.target.map(|v| *v)
        );
        let child_environment: String = connection
            .query_row(
                "SELECT environment_id FROM environment_bindings WHERE session_id=?1",
                [&child.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(environment_id, child_environment);
        let physical = read_environment(&connection, &environment_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            physical.placement.project_directory,
            parent.project_directory
        );
        assert_eq!(physical.placement.target, parent.target.map(|v| *v));
        assert_eq!(
            read_environment_workers(&connection, &environment_id)
                .unwrap()
                .len(),
            2
        );
        connection
            .execute("DELETE FROM sessions WHERE session_id=?1", [&child.id])
            .unwrap();
        connection
            .execute("DELETE FROM sessions WHERE session_id=?1", [&parent.id])
            .unwrap();
        drop(connection);
        let connection = super::super::schema::open_writer(&path).unwrap();
        assert_eq!(
            read_environment_workers(&connection, &environment_id)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            read_environment(&connection, &environment_id)
                .unwrap()
                .unwrap()
                .owner_session_id,
            parent.id
        );
        let prepared_json: String = connection.query_row("SELECT placement_json FROM environments WHERE json_extract(placement_json,'$.prepared_destination.operation_id')='old-move'", [], |row| row.get(0)).unwrap();
        let prepared_owner: EnvironmentPlacement = serde_json::from_str(&prepared_json).unwrap();
        assert_eq!(
            prepared_owner
                .prepared_destination
                .unwrap()
                .destination
                .instance_id(),
            Some("i-fixture")
        );
        assert_eq!(
            read_environment_workers(&connection, &source_environment)
                .unwrap()
                .len(),
            1
        );
        let state = super::super::schema::read_schema_state(&connection).unwrap();
        assert!(state.ensure_supported_by(121).is_err());
    }
}

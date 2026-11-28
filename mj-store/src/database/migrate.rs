//! Inspection of durable state before a controller host migration.

use super::*;
use mj_core::config::DataLayout;
use mj_core::state::StoredMoveOperation;

mod rewrite;
pub use rewrite::*;

#[derive(Debug, Default)]
pub struct MigrationPreconditions {
    pub blockers: Vec<String>,
    pub carried_moves: usize,
    pub waiting_worker_restart_intents: usize,
}

#[derive(Debug)]
pub struct UnidentifiedPlacement {
    pub table: &'static str,
    pub session_id: String,
    pub kind: String,
    pub references_ec2_path: bool,
}

#[derive(Debug, Default)]
pub struct StoreMigrationInventory {
    pub session_targets_by_kind: BTreeMap<String, usize>,
    pub session_targets_by_kind_and_state: BTreeMap<(String, String), usize>,
    /// Recorded local runtime kinds, including leases from removed templates.
    pub local_template_kinds: BTreeMap<String, BTreeSet<String>>,
    pub remote_template_ids: BTreeSet<String>,
    /// Kind, environment phase, and whether this is the current lease.
    pub environment_workers_by_kind_and_phase: BTreeMap<(String, String, bool), usize>,
    pub unidentified_placements: Vec<UnidentifiedPlacement>,
    pub retained_move_sources: usize,
    pub host_snapshot_cache: usize,
    pub host_container_sizes: usize,
    pub mount_history: usize,
    pub project_locations: usize,
    pub detached_drafts: usize,
    pub managed_checkpoints: usize,
    pub external_checkpoints: usize,
}

pub fn inspect_migration_inventory(connection: &Connection) -> Result<StoreMigrationInventory> {
    let mut inventory = StoreMigrationInventory::default();
    let mut query =
        connection.prepare("SELECT kind,count(*) FROM session_targets GROUP BY kind")?;
    for row in query.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))? {
        let (kind, count) = row?;
        inventory.session_targets_by_kind.insert(kind, count);
    }
    let mut query = connection.prepare("SELECT t.kind,s.state,count(*) FROM session_targets t JOIN sessions s ON s.session_id=t.session_id GROUP BY t.kind,s.state")?;
    for row in query.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))? {
        let (kind, state, count) = row?;
        inventory
            .session_targets_by_kind_and_state
            .insert((kind, state), count);
    }
    let mut query = connection.prepare("SELECT s.target_template_id,t.kind FROM sessions s JOIN session_targets t USING(session_id) UNION SELECT target_template_id,kind FROM environment_workers")?;
    for row in query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, kind) = row?;
        record_template_kind(&mut inventory, &id, &kind);
    }
    let mut query = connection.prepare("SELECT s.target_template_id,s.target_runtime_json FROM sessions s LEFT JOIN session_targets t ON t.session_id=s.session_id WHERE t.session_id IS NULL AND s.target_runtime_json IS NOT NULL")?;
    for row in query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, json) = row?;
        let runtime: mj_core::state::TargetRuntimeSettings = serde_json::from_str(&json)?;
        record_template_kind(&mut inventory, &id, &runtime.kind);
    }
    let mut query = connection.prepare("SELECT session_id,operation_json FROM session_moves")?;
    for row in query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (session_id, json) = row?;
        let normalized = session_move::normalize_stored_move(connection, &session_id, &json)?;
        let (
            source_id,
            source_target,
            destination_id,
            destination_target,
            prepared,
            recovery,
            accepted,
        ) = match StoredMoveOperation::decode_json(&normalized)? {
            StoredMoveOperation::Current(op) => (
                op.source_target_template_id,
                op.source_target,
                op.selection.target_template_id,
                op.destination_target,
                op.prepared_destination,
                op.recovery_session,
                op.accepted_preparation,
            ),
            StoredMoveOperation::Legacy(op) => (
                op.source_target_template_id,
                op.source_target,
                op.selection
                    .and_then(|selection| selection.target_template_id),
                op.destination_target,
                op.prepared_destination,
                op.recovery_session,
                None,
            ),
        };
        let source_kind = source_target
            .as_ref()
            .map(mj_core::targets::locator_kind_name);
        if let Some(kind) = source_kind {
            record_template_kind(&mut inventory, &source_id, kind);
        }
        let mut destination_kinds = BTreeSet::new();
        if let Some(target) = &destination_target {
            destination_kinds.insert(mj_core::targets::locator_kind_name(target));
        }
        if let Some(prepared) = &prepared {
            destination_kinds.insert(prepared.runtime.kind.as_str());
            if let Some(target) = prepared.target() {
                destination_kinds.insert(mj_core::targets::locator_kind_name(target));
            }
        }
        if let Some(id) = &destination_id {
            for kind in &destination_kinds {
                record_template_kind(&mut inventory, id, kind);
            }
        }
        if let Some(accepted) = accepted {
            if let Some(kind) = source_kind {
                record_template_kind(&mut inventory, &accepted.source_target_template_id, kind);
            }
            if let Some(id) = accepted.selection.target_template_id {
                for kind in &destination_kinds {
                    record_template_kind(&mut inventory, &id, kind);
                }
            }
        }
        if let Some(session) = recovery {
            record_session_template(&mut inventory, &session);
        }
    }
    let mut query = connection.prepare("SELECT source_json FROM retained_move_sources")?;
    for row in query.query_map([], |row| row.get::<_, String>(0))? {
        let session: SessionRecord = serde_json::from_str(&row?)?;
        record_session_template(&mut inventory, &session);
    }
    let mut query = connection
        .prepare("SELECT placement_json FROM environments WHERE target_lease_id IS NULL")?;
    for row in query.query_map([], |row| row.get::<_, String>(0))? {
        let placement: serde_json::Value = serde_json::from_str(&row?)?;
        let mut kinds = Vec::new();
        if let Some(runtime) = placement.get("runtime").filter(|value| !value.is_null()) {
            let runtime: mj_core::state::TargetRuntimeSettings =
                serde_json::from_value(runtime.clone())?;
            kinds.push(runtime.kind);
        }
        if let Some(prepared) = placement
            .get("prepared_destination")
            .and_then(|value| value.get("destination"))
            .filter(|value| !value.is_null())
        {
            let prepared: mj_core::state::PreparedMoveDestination =
                serde_json::from_value(prepared.clone())?;
            kinds.push(
                prepared
                    .target()
                    .map(mj_core::targets::locator_kind_name)
                    .unwrap_or(&prepared.runtime.kind)
                    .to_owned(),
            );
        }
        if !kinds.is_empty() {
            let id = placement
                .get("target_template_id")
                .and_then(serde_json::Value::as_str)
                .context("environment placement with runtime has no template ID")?;
            for kind in kinds {
                record_template_kind(&mut inventory, id, &kind);
            }
        }
    }
    let mut query = connection.prepare("SELECT w.kind,e.phase,coalesce(e.target_lease_id=w.lease_id,0),count(*) FROM environment_workers w JOIN environments e ON e.environment_id=w.environment_id GROUP BY w.kind,e.phase,3")?;
    for row in query.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    })? {
        let (kind, phase, current, count) = row?;
        inventory
            .environment_workers_by_kind_and_phase
            .insert((kind, phase, current), count);
    }
    for (table, sql) in [
        (
            "session_targets",
            "SELECT t.session_id,t.kind,s.target_runtime_json FROM session_targets t LEFT JOIN sessions s ON s.session_id=t.session_id WHERE t.machine_id IS NULL",
        ),
        (
            "environment_workers",
            "SELECT worker_session_id,kind,runtime_json FROM environment_workers WHERE machine_id IS NULL",
        ),
    ] {
        let mut query = connection.prepare(sql)?;
        for row in query.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })? {
            let (session_id, kind, runtime) = row?;
            inventory
                .unidentified_placements
                .push(UnidentifiedPlacement {
                    table,
                    session_id,
                    kind,
                    references_ec2_path: runtime
                        .as_deref()
                        .map(runtime_references_ec2_path)
                        .transpose()?
                        .unwrap_or(false),
                });
        }
    }
    for (table, count) in [
        (
            "retained_move_sources",
            &mut inventory.retained_move_sources,
        ),
        ("host_snapshot_cache", &mut inventory.host_snapshot_cache),
        ("host_container_sizes", &mut inventory.host_container_sizes),
        ("mount_history", &mut inventory.mount_history),
        ("project_locations", &mut inventory.project_locations),
        ("detached_drafts", &mut inventory.detached_drafts),
    ] {
        *count = connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })?;
    }
    let mut query = connection.prepare("SELECT archive_path FROM session_checkpoints")?;
    for row in query.query_map([], |row| row.get::<_, Vec<u8>>(0))? {
        let path = blob_to_path(&row?);
        if matches!(
            path.components().collect::<Vec<_>>().as_slice(),
            [std::path::Component::Normal(_)]
        ) {
            inventory.managed_checkpoints += 1;
        } else {
            inventory.external_checkpoints += 1;
        }
    }
    Ok(inventory)
}

fn record_template_kind(inventory: &mut StoreMigrationInventory, id: &str, kind: &str) {
    if rewrite::local_kind(kind) {
        inventory
            .local_template_kinds
            .entry(id.to_owned())
            .or_default()
            .insert(kind.to_owned());
    } else {
        inventory.remote_template_ids.insert(id.to_owned());
    }
}

fn record_session_template(inventory: &mut StoreMigrationInventory, session: &SessionRecord) {
    let kind = session
        .target
        .as_deref()
        .map(mj_core::targets::locator_kind_name)
        .or_else(|| {
            session
                .target_runtime
                .as_deref()
                .map(|runtime| runtime.kind.as_str())
        });
    if let Some(kind) = kind {
        record_template_kind(inventory, &session.target_template_id, kind);
    }
}

fn mixed_template_kinds(local_count: usize, remote: bool) -> bool {
    local_count > 1 || (local_count > 0 && remote)
}

fn mixed_template(
    id: &str,
    local: &BTreeMap<String, BTreeSet<String>>,
    remote: &BTreeSet<String>,
) -> bool {
    local
        .get(id)
        .is_some_and(|kinds| mixed_template_kinds(kinds.len(), remote.contains(id)))
}

fn runtime_references_ec2_path(json: &str) -> Result<bool> {
    use mj_core::state::{TargetConnection, TargetRuntimeSettings};
    let runtime: TargetRuntimeSettings = serde_json::from_str(json)?;
    let has_ec2 = |path: &Path| path.components().any(|part| part.as_os_str() == "ec2");
    let args_have_ec2 = |args: &[String]| {
        args.iter().any(|arg| {
            let value = arg
                .strip_prefix("UserKnownHostsFile=")
                .or_else(|| arg.strip_prefix("-oUserKnownHostsFile="))
                .or_else(|| arg.strip_prefix("-i"))
                .unwrap_or(arg);
            has_ec2(Path::new(value))
        })
    };
    Ok(match runtime.connection {
        TargetConnection::Local => false,
        TargetConnection::Ssh { ssh } => {
            ssh.identity_file.as_deref().is_some_and(has_ec2) || args_have_ec2(&ssh.extra_args)
        }
        TargetConnection::Aws {
            identity_file,
            ssh_args,
            ..
        } => identity_file.as_deref().is_some_and(has_ec2) || args_have_ec2(&ssh_args),
    })
}

fn unresolved_move_template_ids(operation: &StoredMoveOperation) -> Vec<&str> {
    let mut ids = Vec::new();
    match operation {
        StoredMoveOperation::Current(op) if !op.is_resolved() => {
            if op.source_target.is_none() {
                ids.push(op.source_target_template_id.as_str());
                if let Some(preparation) = &op.accepted_preparation {
                    ids.push(preparation.source_target_template_id.as_str());
                }
            }
            if op.destination_target.is_none() && op.prepared_destination.is_none() {
                ids.extend(op.selection.target_template_id.as_deref());
                ids.extend(
                    op.accepted_preparation
                        .as_ref()
                        .and_then(|p| p.selection.target_template_id.as_deref()),
                );
            }
        }
        StoredMoveOperation::Legacy(op) if !op.is_resolved() => {
            if op.source_target.is_none() {
                ids.push(op.source_target_template_id.as_str());
            }
            if op.destination_target.is_none() && op.prepared_destination.is_none() {
                ids.extend(
                    op.selection
                        .as_ref()
                        .and_then(|selection| selection.target_template_id.as_deref()),
                );
            }
        }
        _ => {}
    }
    ids
}

/// Inspect semantic ownership, rather than treating every durable intent as active.
pub fn inspect_migration_preconditions(
    connection: &Connection,
    old_data_root: &Path,
) -> Result<MigrationPreconditions> {
    inspect_migration_preconditions_with_remote_templates(
        connection,
        old_data_root,
        &BTreeSet::new(),
    )
}

pub fn inspect_migration_preconditions_with_remote_templates(
    connection: &Connection,
    old_data_root: &Path,
    extra_remote_templates: &BTreeSet<String>,
) -> Result<MigrationPreconditions> {
    inspect_migration_preconditions_with_templates(
        connection,
        old_data_root,
        &BTreeMap::new(),
        extra_remote_templates,
    )
}

pub fn inspect_migration_preconditions_with_templates(
    connection: &Connection,
    old_data_root: &Path,
    extra_local_templates: &BTreeMap<String, BTreeSet<String>>,
    extra_remote_templates: &BTreeSet<String>,
) -> Result<MigrationPreconditions> {
    // Inventory and ownership must describe the same snapshot. A caller may
    // already hold the cutover transaction, in which case it owns that snapshot.
    let snapshot = if connection.is_autocommit() {
        Some(Transaction::new_unchecked(
            connection,
            rusqlite::TransactionBehavior::Deferred,
        )?)
    } else {
        None
    };
    let mut result = MigrationPreconditions::default();
    let inventory = inspect_migration_inventory(connection)?;
    let mut template_kinds = inventory.local_template_kinds;
    for (id, kinds) in extra_local_templates {
        template_kinds
            .entry(id.clone())
            .or_default()
            .extend(kinds.iter().cloned());
    }
    let mut remote_templates = inventory.remote_template_ids;
    remote_templates.extend(extra_remote_templates.iter().cloned());
    let mut moves = connection.prepare(
        "SELECT m.session_id,m.operation_id,m.operation_json,m.close_requested_operation_id,s.state
         FROM session_moves m LEFT JOIN sessions s ON s.session_id=m.session_id ORDER BY m.session_id",
    )?;
    let rows = moves.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    for row in rows {
        let (session, row_id, json, close_id, state) = row?;
        let normalized = session_move::normalize_stored_move(connection, &session, &json)?;
        let op = StoredMoveOperation::decode_json(&normalized)
            .with_context(|| format!("decode Move for {session}"))?;
        let state = match state.as_deref() {
            Some("closing") => Some(SessionState::Closing),
            Some("destroying") => Some(SessionState::Destroying),
            _ => None,
        };
        let exact_close =
            session_move::exact_move_close_intent(op.operation_id(), &row_id, close_id.as_deref());
        let owned = move_recovery_owner(state, exact_close).is_some();
        let active = match &op {
            StoredMoveOperation::Current(op) => {
                op.is_active() || op.awaits_worker_reload() || op.holds_source_environment()
            }
            StoredMoveOperation::Legacy(op) => op.needs_recovery(),
        };
        let ambiguous: BTreeSet<&str> = unresolved_move_template_ids(&op)
            .into_iter()
            .filter(|id| mixed_template(id, &template_kinds, &remote_templates))
            .collect();
        if active || owned {
            result.blockers.push(format!(
                "Move {row_id} for session {session} is active or owned by recovery"
            ));
        }
        for id in &ambiguous {
            result.blockers.push(format!("Move {row_id} for session {session} has ambiguous historical template {id:?} without a recorded locator"));
        }
        if !active && !owned && ambiguous.is_empty() {
            result.carried_moves += 1;
        }
    }
    let mut discovery = connection.prepare("SELECT d.sequence,d.target_template_id,s.target_template_id,t.kind,s.target_runtime_json FROM project_discovery_changes d LEFT JOIN sessions s ON s.session_id=d.session_id LEFT JOIN session_targets t ON t.session_id=d.session_id WHERE d.sequence > (SELECT sequence FROM project_discovery_progress WHERE singleton=1) OR EXISTS(SELECT 1 FROM project_discovery_failures f WHERE f.sequence=d.sequence) ORDER BY d.sequence")?;
    for row in discovery.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })? {
        let (sequence, id, current, target_kind, runtime) = row?;
        if mixed_template(&id, &template_kinds, &remote_templates) {
            let kinds = &template_kinds[&id];
            let kind = if let Some(kind) = target_kind {
                Some(kind)
            } else {
                runtime
                    .as_deref()
                    .map(serde_json::from_str::<mj_core::state::TargetRuntimeSettings>)
                    .transpose()?
                    .map(|runtime| runtime.kind)
            };
            if current.as_deref() != Some(id.as_str())
                || !kind
                    .as_ref()
                    .is_some_and(|kind| !rewrite::local_kind(kind) || kinds.contains(kind))
            {
                result.blockers.push(format!("project discovery change {sequence} has ambiguous historical template {id:?} without a matching current placement"));
            }
        }
    }
    result.waiting_worker_restart_intents =
        connection.query_row("SELECT count(*) FROM worker_restart_intents", [], |row| {
            row.get(0)
        })?;
    for table in [
        "session_restart_intents",
        "session_create_intents",
        "checkpoint_operations",
    ] {
        let mut query = connection.prepare(&format!(
            "SELECT session_id FROM {table} ORDER BY session_id"
        ))?;
        for id in query.query_map([], |row| row.get::<_, String>(0))? {
            result.blockers.push(format!("{table}: {}", id?));
        }
    }
    let mut query = connection.prepare("SELECT session_id,state FROM sessions WHERE state IN ('provisioning','startup-cleanup','checkpointing','closing','destroying') ORDER BY session_id")?;
    for row in query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, state) = row?;
        result.blockers.push(format!("session {id} is {state}"));
    }
    let mut query = connection.prepare("SELECT environment_id,phase FROM environments WHERE phase NOT IN ('active','reclaimed','retiring') ORDER BY environment_id")?;
    for row in query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, phase) = row?;
        result.blockers.push(format!("environment {id} is {phase}"));
    }
    let layout = DataLayout::at(old_data_root.to_path_buf());
    for (table, id_column) in [
        ("session_targets", "session_id"),
        ("environment_workers", "worker_session_id"),
    ] {
        let mut query = connection.prepare(&format!(
            "SELECT {id_column},workspace FROM {table} WHERE kind='local-bare'"
        ))?;
        for row in query.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<Vec<u8>>>(1)?))
        })? {
            let (id, workspace) = row?;
            if workspace.as_deref().map(blob_to_path).as_deref()
                != Some(layout.worker_root(&id).as_path())
            {
                result.blockers.push(format!(
                    "{table} local-bare worker root for {id} does not match the source layout"
                ));
            }
        }
    }
    if let Some(snapshot) = snapshot {
        snapshot.commit()?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preconditions_allow_waiting_worker_restart_but_block_transitions_and_bad_roots() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("store.sqlite3");
        save_session_to(&path, &super::super::tests::session("one", "project")).unwrap();
        let connection = Connection::open(path).unwrap();
        let worker_root = DataLayout::at(root.path()).worker_root("one");
        connection.execute("INSERT OR REPLACE INTO session_targets(session_id,kind,workspace) VALUES ('one','local-bare',?1)", [path_to_blob(&worker_root)]).unwrap();
        connection.execute("INSERT INTO worker_restart_intents(session_id,operation_id,target_json,desired_build,phase) VALUES ('one','restart','{}','build','prepared')", []).unwrap();
        connection.execute("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES ('retired','one','retiring','{}')",[]).unwrap();
        let ready = inspect_migration_preconditions(&connection, root.path()).unwrap();
        assert!(ready.blockers.is_empty(), "{:?}", ready.blockers);
        assert_eq!(ready.waiting_worker_restart_intents, 1);
        let inventory = inspect_migration_inventory(&connection).unwrap();
        assert_eq!(
            inventory.session_targets_by_kind.get("local-bare"),
            Some(&1)
        );
        assert!(
            inventory
                .unidentified_placements
                .iter()
                .any(|item| item.session_id == "one" && item.table == "session_targets")
        );
        connection
            .execute(
                "UPDATE sessions SET state='checkpointing' WHERE session_id='one'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE session_targets SET workspace=?1 WHERE session_id='one'",
                [path_to_blob(Path::new("/wrong"))],
            )
            .unwrap();
        connection.execute("INSERT INTO session_create_intents(session_id,operation_id,remember_launch_choices,created_at) VALUES ('one','create',0,'now')", []).unwrap();
        // The current schema cannot store `provisioning`; the precondition
        // query still guards it so a row from an older revision blocks.
        connection
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        connection.execute("INSERT INTO environments(environment_id,owner_session_id,phase,placement_json) VALUES ('provisioning','one','provisioning','{}')",[]).unwrap();
        connection
            .execute_batch("PRAGMA ignore_check_constraints=OFF")
            .unwrap();
        let blocked = inspect_migration_preconditions(&connection, root.path()).unwrap();
        assert_eq!(blocked.blockers.len(), 4, "{:?}", blocked.blockers);
        assert!(
            blocked
                .blockers
                .iter()
                .any(|blocker| blocker == "environment provisioning is provisioning")
        );
        assert!(
            !blocked
                .blockers
                .iter()
                .any(|blocker| blocker.contains("retired"))
        );
    }
}

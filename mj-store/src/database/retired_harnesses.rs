//! Read and remove sessions whose stored harness no longer has a runtime.
use super::*;
use mj_core::config::HarnessKind;
use mj_core::snapshot_map::SnapshotMap;

#[derive(Clone, Debug)]
pub struct RetiredHarnessSession {
    pub session_id: String,
    pub harness_kind: String,
    pub title: String,
    pub state: SessionState,
    pub target_template_id: String,
    pub resource_allocation: Option<SessionResourceAllocation>,
    /// Prefer an outstanding environment over a reclaimed one. An interrupted
    /// Move can leave multiple placements; teardown checks them all.
    pub owned_environment: Option<EnvironmentRecord>,
}

const SESSION_FIELDS: &str =
    "SELECT session_id, harness_kind, title, state, target_template_id, resource_allocation
     FROM sessions";

fn read_sessions(connection: &Connection, id: Option<&str>) -> Result<Vec<RetiredHarnessSession>> {
    let suffix = if id.is_some() {
        "WHERE session_id=?1"
    } else {
        "ORDER BY session_id"
    };
    let mut statement = connection.prepare(&format!("{SESSION_FIELDS} {suffix}"))?;
    let mut rows = statement.query(rusqlite::params_from_iter(id))?;
    let mut retired = Vec::new();
    while let Some(row) = rows.next()? {
        let harness_kind: String = row.get(1)?;
        if harness_kind.parse::<HarnessKind>().is_ok() {
            continue;
        }
        let session_id: String = row.get(0)?;
        let owned_id: Option<String> = connection
            .query_row(
                "SELECT environment_id FROM environments WHERE owner_session_id=?1
                 ORDER BY phase='reclaimed', rowid DESC LIMIT 1",
                [&session_id],
                |row| row.get(0),
            )
            .optional()?;
        let owned_environment = owned_id
            .map(|id| environments::read_environment(connection, &id))
            .transpose()?
            .flatten();
        let resource_allocation = resource_allocation(&session_id, row.get(5)?);
        retired.push(RetiredHarnessSession {
            session_id,
            harness_kind,
            title: row.get(2)?,
            state: values::stored_session_state(&row.get::<_, String>(3)?),
            target_template_id: row.get(4)?,
            resource_allocation,
            owned_environment,
        });
    }
    Ok(retired)
}

fn resource_allocation(
    session_id: &str,
    stored: Option<String>,
) -> Option<SessionResourceAllocation> {
    stored.and_then(|json| match serde_json::from_str(&json) {
        Ok(allocation) => Some(allocation),
        Err(error) => {
            tracing::warn!(session_id, %error, "retired-harness resource allocation is unreadable");
            None
        }
    })
}

pub fn load_retired_harness_sessions() -> Result<Vec<RetiredHarnessSession>> {
    let mut connection = open_reader(&database_path())?;
    let snapshot = connection.transaction()?;
    read_sessions(&snapshot, None)
}

pub fn load_retired_harness_session(session_id: &str) -> Result<Option<RetiredHarnessSession>> {
    let mut connection = open_reader(&database_path())?;
    let snapshot = connection.transaction()?;
    Ok(read_sessions(&snapshot, Some(session_id))?.pop())
}

fn harness_is_retired(connection: &Connection, id: &str) -> Result<bool> {
    let harness: Option<String> = connection
        .query_row(
            "SELECT harness_kind FROM sessions WHERE session_id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(harness.is_some_and(|kind| kind.parse::<HarnessKind>().is_err()))
}

fn children(connection: &Connection, id: &str) -> Result<Vec<String>> {
    Ok(connection
        .prepare("SELECT child_session_id FROM subagent_sessions WHERE parent_session_id=?1 ORDER BY child_session_id")?
        .query_map([id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Check the entire subtree before any teardown. Old relations may be absent
/// from the controller's typed snapshot, so read the durable ancestry.
pub fn retired_harness_destroy_order(session_id: &str) -> Result<Vec<String>> {
    let mut connection = open_reader(&database_path())?;
    let snapshot = connection.transaction()?;
    destroy_order_with(&snapshot, session_id)
}

fn destroy_order_with(connection: &Connection, session_id: &str) -> Result<Vec<String>> {
    let mut pending = vec![session_id.to_owned()];
    let mut seen = BTreeSet::new();
    let mut order = Vec::new();
    while let Some(id) = pending.pop() {
        ensure!(seen.insert(id.clone()), "cyclic sub-agent ancestry at {id}");
        if !harness_is_retired(connection, &id)? {
            return Err(mj_core::refusal::Refusal::precondition(format!(
                "Cannot destroy retired-harness session {session_id} while supported-harness child {id} depends on it."
            )).into());
        }
        pending.extend(children(connection, &id)?);
        order.push(id);
    }
    order.reverse();
    Ok(order)
}

fn owns_unreclaimed_environment(connection: &Connection, id: &str) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM environments WHERE owner_session_id=?1 AND phase<>'reclaimed')",
        [id],
        |row| row.get(0),
    )?)
}

/// Retain a stopped retired borrower when its row still owns resources or is
/// needed by an independent child. Unbind it without removing worker leases,
/// which remain the shared environment owner's cleanup responsibility.
pub fn retain_stopped_retired_borrower(session_id: &str) -> Result<bool> {
    let id = session_id.to_owned();
    submit_database_write("settle stopped retired borrower", move |connection| {
        let transaction = connection.transaction()?;
        ensure!(
            harness_is_retired(&transaction, &id)?,
            "session {id} is not retired"
        );
        let retain = !children(&transaction, &id)?.is_empty()
            || owns_unreclaimed_environment(&transaction, &id)?;
        if retain {
            transaction.execute(
                "DELETE FROM environment_bindings WHERE session_id=?1 AND environment_id IN
                 (SELECT environment_id FROM environments WHERE owner_session_id<>?1)",
                [&id],
            )?;
        }
        transaction.commit()?;
        Ok(retain)
    })
}

pub fn delete_retired_harness_session(session_id: &str) -> Result<bool> {
    let session_id = session_id.to_owned();
    submit_database_write("delete retired-harness session", move |connection| {
        remove_session_with(connection, &database_path(), &session_id)
    })
}

fn remove_session_with(connection: &mut Connection, path: &Path, id: &str) -> Result<bool> {
    mj_core::config::validate_id("session", id)?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if !harness_is_retired(&transaction, id)? {
        return Ok(false);
    }
    let children = children(&transaction, id)?;
    ensure!(
        children.is_empty(),
        "cannot delete retired-harness session {id} while it has child sessions: {}",
        children.join(", ")
    );
    ensure!(
        !owns_unreclaimed_environment(&transaction, id)?,
        "retired-harness session {id} still owns an unreclaimed environment"
    );
    transaction.execute("DELETE FROM sessions WHERE session_id=?1", [id])?;
    transaction.commit()?;
    sessions::remove_deleted_session_tool_details(path, id);
    Ok(true)
}

type ContainerAllocations = Vec<(String, Option<SessionResourceAllocation>)>;

/// Every retired session, including those with no remaining container allocations.
pub type RetiredContainerAllocations = SnapshotMap<String, ContainerAllocations>;

pub(super) fn container_allocations_with(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<ContainerAllocations>> {
    if !harness_is_retired(connection, session_id)? {
        return Ok(None);
    }
    let allocation: Option<String> = connection.query_row(
        "SELECT resource_allocation FROM sessions WHERE session_id=?1",
        [session_id],
        |row| row.get(0),
    )?;
    let allocation = resource_allocation(session_id, allocation);
    let mut query = connection.prepare(
        "SELECT w.target_template_id, w.kind, w.host, w.resource_id, w.address, w.workspace,
                w.worker_id, w.workspace_storage, w.borrowed_from
         FROM environments e JOIN environment_workers w ON w.lease_id=e.target_lease_id
         WHERE e.owner_session_id=?1 AND e.phase<>'reclaimed' ORDER BY e.environment_id",
    )?;
    let mut rows = query.query([session_id])?;
    let mut containers = Vec::new();
    while let Some(row) = rows.next()? {
        if state_io::decode_target(row, 1)?.owns_container() {
            containers.push((row.get(0)?, allocation.clone()));
        }
    }
    Ok(Some(containers))
}

pub(super) fn load_container_allocations(
    connection: &Connection,
) -> Result<RetiredContainerAllocations> {
    let mut capacity = RetiredContainerAllocations::new();
    let mut query = connection.prepare("SELECT session_id, harness_kind FROM sessions")?;
    let mut rows = query.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)?.parse::<HarnessKind>().is_ok() {
            continue;
        }
        let id: String = row.get(0)?;
        if let Some(allocations) = container_allocations_with(connection, &id)? {
            capacity.insert(id, allocations);
        }
    }
    Ok(capacity)
}

pub fn load_retired_container_allocations() -> Result<RetiredContainerAllocations> {
    if let Some(committed) = committed_state()? {
        return Ok(committed.retired_container_allocations.clone());
    }
    let mut connection = open_reader(&database_path())?;
    let snapshot = connection.transaction()?;
    load_container_allocations(&snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_harness_deletion_respects_children_and_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("controller.sqlite");
        let (parent, child) = super::super::tests::subagent_pair(&path);
        let mut connection = open(&path).unwrap();

        assert!(!remove_session_with(&mut connection, &path, &child.id).unwrap());
        assert!(read_sessions(&connection, None).unwrap().is_empty());
        connection
            .execute(
                "UPDATE sessions SET harness_kind='retired-harness' WHERE session_id=?1",
                [&parent.id],
            )
            .unwrap();
        let error = destroy_order_with(&connection, &parent.id).unwrap_err();
        assert!(error.to_string().contains(&child.id), "{error:#}");
        connection
            .execute(
                "UPDATE sessions SET harness_kind='retired-harness' WHERE session_id=?1",
                [&child.id],
            )
            .unwrap();
        assert_eq!(
            destroy_order_with(&connection, &parent.id).unwrap(),
            [child.id.clone(), parent.id.clone()]
        );
        let capacities = load_container_allocations(&connection).unwrap();
        assert!(capacities.get(&child.id).unwrap().is_empty());
        let retired = read_sessions(&connection, None).unwrap();
        assert_eq!(retired.len(), 2);
        assert_eq!(retired[0].session_id, parent.id);
        let environment = retired[0].owned_environment.as_ref().unwrap();
        assert_eq!(environment.owner_session_id, parent.id);
        assert_eq!(retired[1].session_id, child.id);
        assert!(retired[1].owned_environment.is_none());

        let error = remove_session_with(&mut connection, &path, &parent.id).unwrap_err();
        assert!(error.to_string().contains(&child.id), "{error:#}");
        assert!(remove_session_with(&mut connection, &path, &child.id).unwrap());
        assert!(
            remove_session_with(&mut connection, &path, &parent.id)
                .unwrap_err()
                .to_string()
                .contains("unreclaimed environment")
        );
        connection
            .execute(
                "UPDATE environments SET phase='reclaimed' WHERE environment_id=?1",
                [&environment.id],
            )
            .unwrap();
        assert!(remove_session_with(&mut connection, &path, &parent.id).unwrap());
        assert!(read_sessions(&connection, None).unwrap().is_empty());
    }
}

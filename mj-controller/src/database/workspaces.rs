use super::*;

/// Every workspace that can hold a session the user sees.
///
/// The store keeps a row with the id `default` that older releases put
/// sessions in when no workspace was named. No new session goes there, and
/// the row is left out only while it holds no session at all: a store whose
/// `default` holds sessions, suspended ones included, lists it as an ordinary
/// workspace named `default`, so none of those sessions is hidden (launch
/// finding H-3).
pub fn list_workspaces() -> Result<Vec<WorkspaceRecord>> {
    list_workspaces_from(&database_path())
}

/// The `default` row keeps its name whether or not it is listed. While it
/// holds no session it is not listed, so creating a workspace with its name
/// would return a workspace nobody can see. Refuse the name then.
fn refuse_legacy_default_name(connection: &Connection, name_key: &str) -> Result<()> {
    let hidden: bool = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM workspaces w
              WHERE w.workspace_id = ?1 AND w.name_key = ?2
                AND NOT EXISTS(
                    SELECT 1 FROM session_contexts c JOIN sessions s USING(session_id)
                     WHERE c.workspace_id = w.workspace_id))",
        [DEFAULT_WORKSPACE_ID, name_key],
        |row| row.get(0),
    )?;
    ensure!(
        !hidden,
        "the workspace name {name_key:?} is reserved for sessions made before a workspace \
         was required; choose another name"
    );
    Ok(())
}

pub(super) struct DbPaneSize(PaneSize);

impl rusqlite::types::ToSql for DbPaneSize {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(match self.0 {
            PaneSize::Minimized => "minimized",
            PaneSize::Standard => "standard",
            PaneSize::Maximized => "maximized",
        }
        .into())
    }
}

impl rusqlite::types::FromSql for DbPaneSize {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        match value.as_str()? {
            "minimized" => Ok(Self(PaneSize::Minimized)),
            "standard" => Ok(Self(PaneSize::Standard)),
            "maximized" => Ok(Self(PaneSize::Maximized)),
            other => Err(rusqlite::types::FromSqlError::Other(
                format!("unknown pane size {other:?}").into(),
            )),
        }
    }
}

pub fn load_workspace_pane_sizes(workspace_id: &str) -> Result<PaneSizes> {
    load_workspace_pane_sizes_from(&database_path(), workspace_id)
}

pub fn load_workspace_pane_sizes_from(path: &Path, workspace_id: &str) -> Result<PaneSizes> {
    let connection = open_reader(path)?;
    let sizes = connection
        .query_row(
            "SELECT coalesce(p.sessions, 'standard'), coalesce(p.targets, 'standard'),
                    coalesce(p.quota, 'standard')
             FROM workspaces w LEFT JOIN workspace_pane_sizes p USING(workspace_id)
             WHERE w.workspace_id = ?1",
            [workspace_id],
            |row| {
                Ok(PaneSizes {
                    sessions: row.get::<_, DbPaneSize>(0)?.0,
                    targets: row.get::<_, DbPaneSize>(1)?.0,
                    quota: row.get::<_, DbPaneSize>(2)?.0,
                })
            },
        )
        .optional()?
        .with_context(|| format!("unknown workspace {workspace_id:?}"))?;
    sizes.validate()?;
    Ok(sizes)
}

pub fn save_workspace_pane_sizes(workspace_id: &str, sizes: PaneSizes) -> Result<()> {
    let workspace_id = workspace_id.to_owned();
    submit_database_write("save_workspace_pane_sizes", move |_| {
        save_workspace_pane_sizes_to(&database_path(), &workspace_id, sizes)
    })
}

pub fn save_workspace_pane_sizes_to(
    path: &Path,
    workspace_id: &str,
    sizes: PaneSizes,
) -> Result<()> {
    sizes.validate()?;
    let connection = open(path)?;
    connection
        .execute(
            "INSERT INTO workspace_pane_sizes(workspace_id, sessions, targets, quota)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(workspace_id) DO UPDATE SET
             sessions = excluded.sessions, targets = excluded.targets, quota = excluded.quota",
            params![
                workspace_id,
                DbPaneSize(sizes.sessions),
                DbPaneSize(sizes.targets),
                DbPaneSize(sizes.quota)
            ],
        )
        .with_context(|| format!("save pane sizes for workspace {workspace_id:?}"))?;
    Ok(())
}

pub fn load_workspace_layout(workspace_id: &str) -> Result<ConversationLayout> {
    load_workspace_layout_from(&database_path(), workspace_id)
}

pub fn load_workspace_layout_from(path: &Path, workspace_id: &str) -> Result<ConversationLayout> {
    let connection = open_reader(path)?;
    let stored = connection
        .query_row(
            "SELECT l.layout
             FROM workspaces w LEFT JOIN workspace_layouts l USING(workspace_id)
             WHERE w.workspace_id = ?1",
            [workspace_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .with_context(|| format!("unknown workspace {workspace_id:?}"))?;
    let Some(stored) = stored else {
        return Ok(ConversationLayout::default());
    };
    let layout: ConversationLayout = serde_json::from_str(&stored)
        .with_context(|| format!("decode layout for workspace {workspace_id:?}"))?;
    layout.validate()?;
    Ok(layout)
}

pub fn save_workspace_layout(workspace_id: &str, layout: ConversationLayout) -> Result<()> {
    let workspace_id = workspace_id.to_owned();
    submit_database_write("save_workspace_layout", move |_| {
        save_workspace_layout_to(&database_path(), &workspace_id, &layout)
    })
}

pub fn save_workspace_layout_to(
    path: &Path,
    workspace_id: &str,
    layout: &ConversationLayout,
) -> Result<()> {
    layout.validate()?;
    let encoded = serde_json::to_string(layout)?;
    let connection = open(path)?;
    connection
        .execute(
            "INSERT INTO workspace_layouts(workspace_id, layout)
         VALUES (?1, ?2)
         ON CONFLICT(workspace_id) DO UPDATE SET layout = excluded.layout",
            params![workspace_id, encoded],
        )
        .with_context(|| format!("save layout for workspace {workspace_id:?}"))?;
    Ok(())
}

pub fn list_workspaces_from(path: &Path) -> Result<Vec<WorkspaceRecord>> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare(
        "SELECT w.workspace_id, w.name, w.created_at, w.last_opened_at,
                count(s.session_id) FILTER (
                    WHERE s.state NOT IN ('stopped', 'lost', 'destroyed-with-data-loss')
                )
          FROM workspaces w
           LEFT JOIN session_contexts c USING(workspace_id)
           LEFT JOIN sessions s USING(session_id)
          GROUP BY w.workspace_id
         HAVING w.workspace_id != 'default' OR count(s.session_id) > 0
          ORDER BY w.last_opened_at DESC, w.created_at DESC, w.workspace_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(WorkspaceRecord {
            id: row.get(0)?,
            name: row.get(1)?,
            created_at: row.get(2)?,
            last_opened_at: row.get(3)?,
            session_count: row.get(4)?,
        })
    })?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

pub fn create_workspace(name: &str) -> Result<WorkspaceRecord> {
    let name = name.to_owned();
    submit_database_write("create_workspace", move |_| {
        create_workspace_at(&database_path(), &name)
    })
}

/// Create the named workspace, or return the concurrently-created winner.
///
/// Interactive setup uses this operation after presenting a snapshot of the
/// workspace list. Several selectors can therefore submit the same normalized
/// name legitimately. Explicit database creation remains strict through
/// [`create_workspace`].
pub fn create_or_get_workspace(name: &str) -> Result<WorkspaceRecord> {
    let name = name.to_owned();
    submit_database_write("create_or_get_workspace", move |_| {
        create_or_get_workspace_at(&database_path(), &name)
    })
}

pub fn create_or_get_workspace_at(path: &Path, name: &str) -> Result<WorkspaceRecord> {
    let (name, name_key) = normalize_workspace_name(name)?;
    let id = new_workspace_id()?;
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut connection = open(path)?;
    refuse_legacy_default_name(&connection, &name_key)?;
    let transaction = connection.transaction()?;
    transaction
        .execute(
            "INSERT INTO workspaces(workspace_id, name, name_key, created_at, last_opened_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(name_key) DO NOTHING",
            params![id, name, name_key, now],
        )
        .with_context(|| format!("create or find workspace {name:?}"))?;
    let workspace = transaction.query_row(
        "SELECT w.workspace_id, w.name, w.created_at, w.last_opened_at,
                count(s.session_id) FILTER (
                    WHERE s.state NOT IN ('stopped', 'lost', 'destroyed-with-data-loss')
                )
           FROM workspaces w
           LEFT JOIN session_contexts c USING(workspace_id)
           LEFT JOIN sessions s USING(session_id)
          WHERE w.name_key = ?1
          GROUP BY w.workspace_id",
        params![name_key],
        |row| {
            Ok(WorkspaceRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: row.get(2)?,
                last_opened_at: row.get(3)?,
                session_count: row.get(4)?,
            })
        },
    )?;
    transaction.commit()?;
    Ok(workspace)
}

pub fn create_workspace_at(path: &Path, name: &str) -> Result<WorkspaceRecord> {
    let (name, name_key) = normalize_workspace_name(name)?;
    let id = new_workspace_id()?;
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let connection = open(path)?;
    refuse_legacy_default_name(&connection, &name_key)?;
    connection
        .execute(
            "INSERT INTO workspaces(workspace_id, name, name_key, created_at, last_opened_at)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            params![id, name, name_key, now],
        )
        .with_context(|| format!("create workspace {name:?}"))?;
    Ok(WorkspaceRecord {
        id,
        name,
        created_at: now.clone(),
        last_opened_at: now,
        session_count: 0,
    })
}

pub fn rename_workspace(workspace_id: &str, name: &str) -> Result<()> {
    let workspace_id = workspace_id.to_owned();
    let name = name.to_owned();
    submit_database_write("rename_workspace", move |_| {
        rename_workspace_at(&database_path(), &workspace_id, &name)
    })
}

pub fn rename_workspace_at(path: &Path, workspace_id: &str, name: &str) -> Result<()> {
    let (name, name_key) = normalize_workspace_name(name)?;
    let connection = open(path)?;
    let changed = connection
        .execute(
            "UPDATE workspaces SET name = ?2, name_key = ?3 WHERE workspace_id = ?1",
            params![workspace_id, name, name_key],
        )
        .with_context(|| format!("rename workspace to {name:?}"))?;
    ensure!(changed == 1, "unknown workspace {workspace_id:?}");
    Ok(())
}

pub fn touch_workspace(workspace_id: &str) -> Result<()> {
    let workspace_id = workspace_id.to_owned();
    submit_database_write("touch_workspace", move |_| {
        touch_workspace_at(&database_path(), &workspace_id)
    })
}

pub fn touch_workspace_at(path: &Path, workspace_id: &str) -> Result<()> {
    let connection = open(path)?;
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let changed = connection.execute(
        "UPDATE workspaces SET last_opened_at = ?2 WHERE workspace_id = ?1",
        params![workspace_id, now],
    )?;
    ensure!(changed == 1, "unknown workspace {workspace_id:?}");
    Ok(())
}

/// Delete a workspace that owns no active sessions or recoverable drafts.
///
/// Inactive session records are global resume history. Their last workspace
/// id is retained as historical metadata even when that workspace disappears.
pub fn delete_workspace(workspace_id: &str) -> Result<()> {
    let workspace_id = workspace_id.to_owned();
    submit_database_write("delete_workspace", move |_| {
        delete_workspace_at(&database_path(), &workspace_id)
    })
}

pub fn delete_workspace_at(path: &Path, workspace_id: &str) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let active_count = {
        let mut statement = tx.prepare(
            "SELECT s.state
               FROM session_contexts c
               JOIN sessions s USING(session_id)
              WHERE c.workspace_id = ?1",
        )?;
        let states = statement.query_map([workspace_id], |row| row.get::<_, String>(0))?;
        states
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|state| stored_session_state(state).is_active())
            .count()
    };
    let draft_count: u64 = tx.query_row(
        "SELECT count(*) FROM detached_drafts WHERE workspace_id = ?1",
        [workspace_id],
        |row| row.get(0),
    )?;
    ensure!(
        active_count == 0 && draft_count == 0,
        "workspace is not empty ({active_count} active sessions, {draft_count} drafts)"
    );
    let changed = tx.execute(
        "DELETE FROM workspaces WHERE workspace_id = ?1",
        [workspace_id],
    )?;
    ensure!(changed == 1, "unknown workspace {workspace_id:?}");
    tx.commit()?;
    Ok(())
}

/// Force-delete a workspace whose active sessions have already been destroyed.
///
/// Drops the workspace's detached drafts and the workspace row in one
/// immediate transaction that re-checks for active sessions, so a session
/// created while the destruction ran refuses the deletion instead of losing
/// the drafts. Inactive session records are global history and are preserved,
/// exactly as in [`delete_workspace`].
pub fn force_delete_workspace(workspace_id: &str) -> Result<()> {
    let workspace_id = workspace_id.to_owned();
    submit_database_write("force_delete_workspace", move |_| {
        force_delete_workspace_at(&database_path(), &workspace_id)
    })
}

pub fn force_delete_workspace_at(path: &Path, workspace_id: &str) -> Result<()> {
    remove_inactive_workspace_at(path, workspace_id, false)
}

/// Finish a normal workspace close, retaining history but discarding unsent text.
pub fn close_workspace(workspace_id: &str) -> Result<()> {
    let workspace_id = workspace_id.to_owned();
    submit_database_write("close_workspace", move |_| {
        close_workspace_at(&database_path(), &workspace_id)
    })
}

pub fn close_workspace_at(path: &Path, workspace_id: &str) -> Result<()> {
    remove_inactive_workspace_at(path, workspace_id, true)
}

fn remove_inactive_workspace_at(
    path: &Path,
    workspace_id: &str,
    discard_session_drafts: bool,
) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let active_count = {
        let mut statement = tx.prepare(
            "SELECT s.state
               FROM session_contexts c
               JOIN sessions s USING(session_id)
              WHERE c.workspace_id = ?1",
        )?;
        let states = statement.query_map([workspace_id], |row| row.get::<_, String>(0))?;
        states
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|state| stored_session_state(state).is_active())
            .count()
    };
    ensure!(
        active_count == 0,
        "workspace is not empty ({active_count} active sessions remain)"
    );
    if discard_session_drafts {
        tx.execute(
            "UPDATE sessions SET draft_input = '' WHERE session_id IN
             (SELECT session_id FROM session_contexts WHERE workspace_id = ?1)",
            [workspace_id],
        )?;
    }
    tx.execute(
        "DELETE FROM detached_drafts WHERE workspace_id = ?1",
        [workspace_id],
    )?;
    let changed = tx.execute(
        "DELETE FROM workspaces WHERE workspace_id = ?1",
        [workspace_id],
    )?;
    ensure!(changed == 1, "unknown workspace {workspace_id:?}");
    tx.commit()?;
    Ok(())
}

/// Move a durable history into the workspace from which it is being resumed.
///
/// This is deliberately limited to states accepted by the resume controller;
/// an active session must never move between live dashboards underneath its
/// worker or viewers.
pub fn reassign_resumable_session_workspace(session_id: &str, workspace_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let workspace_id = workspace_id.to_owned();
    submit_database_write("reassign_resumable_session_workspace", move |_| {
        reassign_resumable_session_workspace_at(&database_path(), &session_id, &workspace_id)
    })
}

pub fn reassign_resumable_session_workspace_at(
    path: &Path,
    session_id: &str,
    workspace_id: &str,
) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (current_workspace, state): (String, String) = tx
        .query_row(
            "SELECT c.workspace_id, s.state
               FROM session_contexts c
               JOIN sessions s USING(session_id)
              WHERE c.session_id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .with_context(|| format!("find resumable session {session_id:?}"))?;
    ensure!(
        matches!(
            stored_session_state(&state),
            SessionState::Stopped | SessionState::Lost | SessionState::Error
        ),
        "session {session_id} is not resumable"
    );
    let destination_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspaces WHERE workspace_id = ?1)",
        [workspace_id],
        |row| row.get(0),
    )?;
    ensure!(destination_exists, "unknown workspace {workspace_id:?}");
    if current_workspace != workspace_id {
        tx.execute(
            "UPDATE session_contexts SET workspace_id = ?2 WHERE session_id = ?1",
            params![session_id, workspace_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn workspace_for_session_at(path: &Path, session_id: &str) -> Result<Option<String>> {
    open_reader(path)?
        .query_row(
            "SELECT workspace_id FROM session_contexts WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

pub fn session_ids_for_workspace(workspace_id: &str) -> Result<Vec<String>> {
    session_ids_for_workspace_at(&database_path(), workspace_id)
}

/// Return sessions whose current or last workspace id matches `workspace_id`.
/// Callers deciding live membership must additionally check `SessionState`.
pub fn session_ids_for_workspace_at(path: &Path, workspace_id: &str) -> Result<Vec<String>> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare(
        "SELECT c.session_id
           FROM session_contexts c
           JOIN sessions s USING(session_id)
          WHERE c.workspace_id = ?1
          ORDER BY c.created_at, c.session_id",
    )?;
    let rows = statement.query_map([workspace_id], |row| row.get(0))?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

/// Assign a newly-created session context to a workspace. Existing contexts
/// remain immutable here; only the guarded resume operation may move one.
pub fn assign_new_session_workspace(session_id: &str, workspace_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let workspace_id = workspace_id.to_owned();
    submit_database_write("assign_new_session_workspace", move |_| {
        assign_new_session_workspace_at(&database_path(), &session_id, &workspace_id)
    })
}

pub fn assign_new_session_workspace_at(
    path: &Path,
    session_id: &str,
    workspace_id: &str,
) -> Result<()> {
    let connection = open(path)?;
    let current: String = connection
        .query_row(
            "SELECT workspace_id FROM session_contexts WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .with_context(|| format!("find session context {session_id:?}"))?;
    if current == workspace_id {
        return Ok(());
    }
    ensure!(
        current == DEFAULT_WORKSPACE_ID,
        "session {session_id} already belongs to workspace {current}"
    );
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspaces WHERE workspace_id = ?1)",
        [workspace_id],
        |row| row.get(0),
    )?;
    ensure!(exists, "unknown workspace {workspace_id:?}");
    connection.execute(
        "UPDATE session_contexts SET workspace_id = ?2 WHERE session_id = ?1",
        params![session_id, workspace_id],
    )?;
    Ok(())
}

use super::*;

pub fn client_read_frontier(client_id: &str, workspace_id: &str, session_id: &str) -> Result<u64> {
    client_read_frontier_at(&database_path(), client_id, workspace_id, session_id)
}

pub(super) fn client_read_frontier_at(
    path: &Path,
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
) -> Result<u64> {
    let connection = open_reader(path)?;
    let client: Option<u64> = connection
        .query_row(
            "SELECT through_event_ordinal
               FROM client_read_frontiers
              WHERE client_id = ?1 AND workspace_id = ?2 AND session_id = ?3",
            params![client_id, workspace_id, session_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(frontier) = client {
        return Ok(frontier);
    }
    connection
        .query_row(
            "SELECT s.viewed_through_event_ordinal
               FROM sessions s JOIN session_contexts c USING(session_id)
              WHERE s.session_id = ?1 AND c.workspace_id = ?2",
            params![session_id, workspace_id],
            |row| row.get(0),
        )
        .with_context(|| format!("find session {session_id:?} in workspace {workspace_id:?}"))
}

pub fn advance_client_read_frontier(
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
    through: u64,
) -> Result<u64> {
    let client_id = client_id.to_owned();
    let workspace_id = workspace_id.to_owned();
    let session_id = session_id.to_owned();
    submit_database_write("advance_client_read_frontier", move |_| {
        advance_client_read_frontier_at(
            &database_path(),
            &client_id,
            &workspace_id,
            &session_id,
            through,
        )
    })
}

/// What this viewer has stored for this session: an unsent draft and how far
/// it has read.
pub fn client_session_state(
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
) -> Result<ClientSessionState> {
    let connection = open_reader(&database_path())?;
    let draft = connection
        .query_row(
            "SELECT draft FROM client_session_state
              WHERE client_id = ?1 AND workspace_id = ?2 AND session_id = ?3",
            params![client_id, workspace_id, session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_default();
    let through_event_ordinal = connection
        .query_row(
            "SELECT through_event_ordinal FROM client_read_frontiers
              WHERE client_id = ?1 AND workspace_id = ?2 AND session_id = ?3",
            params![client_id, workspace_id, session_id],
            |row| row.get::<_, u64>(0),
        )
        .optional()?
        .unwrap_or_default();
    Ok(ClientSessionState {
        draft,
        through_event_ordinal,
    })
}

/// Store one viewer's unsent draft.
///
/// An empty draft deletes the row rather than storing emptiness, so a viewer
/// that cleared its composer stops occupying a row and stops being pruned
/// later for something it no longer holds.
pub fn persist_client_draft(
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
    draft: &str,
) -> Result<()> {
    ensure!(!client_id.trim().is_empty(), "client id is empty");
    let client_id = client_id.to_owned();
    let workspace_id = workspace_id.to_owned();
    let session_id = session_id.to_owned();
    let draft = draft.to_owned();
    submit_database_write("persist_client_draft", move |connection| {
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if draft.is_empty() {
            transaction.execute(
                "DELETE FROM client_session_state
                  WHERE client_id = ?1 AND workspace_id = ?2 AND session_id = ?3",
                params![client_id, workspace_id, session_id],
            )?;
            transaction.commit()?;
            return Ok(());
        }
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let changed = transaction.execute(
            "INSERT INTO client_session_state(
                 client_id, workspace_id, session_id, draft, updated_at
             )
             SELECT ?1, ?2, ?3, ?4, ?5
              WHERE EXISTS(
                  SELECT 1 FROM session_contexts
                   WHERE session_id = ?3 AND workspace_id = ?2
              )
             ON CONFLICT(client_id, workspace_id, session_id) DO UPDATE SET
                 draft = excluded.draft,
                 updated_at = excluded.updated_at",
            params![client_id, workspace_id, session_id, draft, now],
        )?;
        ensure!(
            changed == 1,
            "session {session_id:?} is not in workspace {workspace_id:?}"
        );
        transaction.commit()?;
        Ok(())
    })
}

/// Forget web-viewer state that has passed its retention.
///
/// Only rows whose client id names a phone are considered. A terminal client's
/// read frontier is not the phone's to expire, and deleting one would lose a
/// person's place in a conversation they are still reading.
pub fn prune_phone_client_state(older_than: Duration) -> Result<usize> {
    let cutoff = (Utc::now() - chrono::Duration::from_std(older_than).unwrap_or_default())
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    submit_database_write("prune_phone_client_state", move |connection| {
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let drafts = transaction.execute(
            "DELETE FROM client_session_state
              WHERE client_id LIKE 'phone:%' AND updated_at < ?1",
            params![cutoff],
        )?;
        let frontiers = transaction.execute(
            "DELETE FROM client_read_frontiers
              WHERE client_id LIKE 'phone:%' AND updated_at < ?1",
            params![cutoff],
        )?;
        transaction.commit()?;
        Ok(drafts + frontiers)
    })
}

pub fn persist_read_receipt(
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
    through: u64,
) -> Result<u64> {
    let client_id = client_id.to_owned();
    let workspace_id = workspace_id.to_owned();
    let session_id = session_id.to_owned();
    submit_database_write("persist_read_receipt", move |connection| {
        persist_read_receipt_with(connection, &client_id, &workspace_id, &session_id, through)
    })
}

pub(super) fn persist_read_receipt_with(
    connection: &mut Connection,
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
    through: u64,
) -> Result<u64> {
    ensure!(!client_id.trim().is_empty(), "client id is empty");
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let applied = transaction
        .query_row(
            "SELECT applied_event_ordinal FROM materialized_sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get::<_, u64>(0),
        )
        .optional()?
        .with_context(|| format!("unknown session {session_id}"))?;
    ensure!(
        through <= applied,
        "cannot acknowledge event ordinal {through} for session {session_id}; projection is at {applied}"
    );
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let changed = transaction.execute(
        "INSERT INTO client_read_frontiers(
             client_id, workspace_id, session_id, through_event_ordinal, updated_at
         )
         SELECT ?1, ?2, ?3, ?4, ?5
          WHERE EXISTS(
              SELECT 1 FROM session_contexts
               WHERE session_id = ?3 AND workspace_id = ?2
          )
         ON CONFLICT(client_id, workspace_id, session_id) DO UPDATE SET
             through_event_ordinal = max(
                 client_read_frontiers.through_event_ordinal,
                 excluded.through_event_ordinal
             ),
             updated_at = excluded.updated_at",
        params![client_id, workspace_id, session_id, through, now],
    )?;
    ensure!(
        changed == 1,
        "session {session_id:?} is not in workspace {workspace_id:?}"
    );
    let changed = transaction.execute(
        "UPDATE sessions
         SET viewed_through_event_ordinal = max(viewed_through_event_ordinal, ?2)
         WHERE session_id = ?1",
        params![session_id, through],
    )?;
    ensure!(changed == 1, "unknown session {session_id}");
    let frontier = transaction.query_row(
        "SELECT through_event_ordinal
           FROM client_read_frontiers
          WHERE client_id = ?1 AND workspace_id = ?2 AND session_id = ?3",
        params![client_id, workspace_id, session_id],
        |row| row.get(0),
    )?;
    transaction.commit()?;
    Ok(frontier)
}

pub(super) fn advance_client_read_frontier_at(
    path: &Path,
    client_id: &str,
    workspace_id: &str,
    session_id: &str,
    through: u64,
) -> Result<u64> {
    ensure!(!client_id.trim().is_empty(), "client id is empty");
    let connection = open(path)?;
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let changed = connection.execute(
        "INSERT INTO client_read_frontiers(
             client_id, workspace_id, session_id, through_event_ordinal, updated_at
         )
         SELECT ?1, ?2, ?3, ?4, ?5
          WHERE EXISTS(
              SELECT 1 FROM session_contexts
               WHERE session_id = ?3 AND workspace_id = ?2
          )
         ON CONFLICT(client_id, workspace_id, session_id) DO UPDATE SET
             through_event_ordinal = max(
                 client_read_frontiers.through_event_ordinal,
                 excluded.through_event_ordinal
             ),
             updated_at = excluded.updated_at",
        params![client_id, workspace_id, session_id, through, now],
    )?;
    ensure!(
        changed == 1,
        "session {session_id:?} is not in workspace {workspace_id:?}"
    );
    client_read_frontier_at(path, client_id, workspace_id, session_id)
}

/// Preserve unsent input for explicit recovery and retire its unchanged legacy
/// seed together. Empty input still retires the seed: clearing is an edit.
pub fn save_detached_session_draft(
    workspace_id: &str,
    session_id: &str,
    source: &str,
    owner_pid: u32,
    draft: DetachedSessionDraft,
) -> Result<Option<String>> {
    let workspace_id = workspace_id.to_owned();
    let session_id = session_id.to_owned();
    let source = source.to_owned();
    submit_database_write("save_detached_session_draft", move |connection| {
        save_detached_session_draft_in(
            connection,
            &workspace_id,
            &session_id,
            &source,
            owner_pid,
            &draft,
        )
    })
}

pub(super) fn save_detached_session_draft_in(
    connection: &mut Connection,
    workspace_id: &str,
    session_id: &str,
    source: &str,
    owner_pid: u32,
    draft: &DetachedSessionDraft,
) -> Result<Option<String>> {
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(inherited) = &draft.inherited_input {
        transaction.execute(
            "UPDATE sessions SET draft_input = ''
              WHERE session_id = ?1 AND draft_input = ?2
                AND EXISTS (SELECT 1 FROM session_contexts
                             WHERE session_id = ?1 AND workspace_id = ?3)",
            params![session_id, inherited, workspace_id],
        )?;
    }
    let id = insert_detached_draft(
        &transaction,
        workspace_id,
        Some(session_id),
        source,
        Some(owner_pid),
        &draft.text,
    )?;
    transaction.commit()?;
    Ok(id)
}

pub fn save_detached_draft(
    workspace_id: &str,
    session_id: Option<&str>,
    source: &str,
    owner_pid: Option<u32>,
    text: &str,
) -> Result<Option<String>> {
    let workspace_id = workspace_id.to_owned();
    let session_id = session_id.map(str::to_owned);
    let source = source.to_owned();
    let text = text.to_owned();
    submit_database_write("save_detached_draft", move |_| {
        save_detached_draft_at(
            &database_path(),
            &workspace_id,
            session_id.as_deref(),
            &source,
            owner_pid,
            &text,
        )
    })
}

pub(super) fn save_detached_draft_at(
    path: &Path,
    workspace_id: &str,
    session_id: Option<&str>,
    source: &str,
    owner_pid: Option<u32>,
    text: &str,
) -> Result<Option<String>> {
    if text.is_empty() {
        return Ok(None);
    }
    let connection = open(path)?;
    insert_detached_draft(
        &connection,
        workspace_id,
        session_id,
        source,
        owner_pid,
        text,
    )
}

pub(super) fn insert_detached_draft(
    connection: &Connection,
    workspace_id: &str,
    session_id: Option<&str>,
    source: &str,
    owner_pid: Option<u32>,
    text: &str,
) -> Result<Option<String>> {
    if text.is_empty() {
        return Ok(None);
    }
    ensure!(!source.trim().is_empty(), "draft source is empty");
    let id = new_workspace_id()?;
    let saved_at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    connection.execute(
        "INSERT INTO detached_drafts(
             draft_id, workspace_id, session_id, source, owner_pid, saved_at, text
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            workspace_id,
            session_id,
            source,
            owner_pid,
            saved_at,
            text
        ],
    )?;
    Ok(Some(id))
}

pub fn list_detached_drafts(workspace_id: &str) -> Result<Vec<DetachedDraft>> {
    list_detached_drafts_at(&database_path(), workspace_id)
}

pub(super) fn list_detached_drafts_at(
    path: &Path,
    workspace_id: &str,
) -> Result<Vec<DetachedDraft>> {
    let connection = open_reader(path)?;
    let mut statement = connection.prepare(
        "SELECT draft_id, workspace_id, session_id, source, owner_pid, saved_at, text,
                recovered_at
           FROM detached_drafts
          WHERE workspace_id = ?1 AND recovered_at IS NULL
          ORDER BY saved_at DESC, draft_id DESC",
    )?;
    let rows = statement.query_map([workspace_id], |row| {
        Ok(DetachedDraft {
            id: row.get(0)?,
            workspace_id: row.get(1)?,
            session_id: row.get(2)?,
            source: row.get(3)?,
            owner_pid: row.get(4)?,
            saved_at: row.get(5)?,
            text: row.get(6)?,
            recovered_at: row.get(7)?,
        })
    })?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

pub fn mark_draft_recovered(draft_id: &str) -> Result<()> {
    let draft_id = draft_id.to_owned();
    submit_database_write("mark_draft_recovered", move |_| {
        mark_draft_recovered_at(&database_path(), &draft_id)
    })
}

pub(super) fn mark_draft_recovered_at(path: &Path, draft_id: &str) -> Result<()> {
    let connection = open(path)?;
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let changed = connection.execute(
        "UPDATE detached_drafts SET recovered_at = ?2
          WHERE draft_id = ?1 AND recovered_at IS NULL",
        params![draft_id, now],
    )?;
    ensure!(
        changed == 1,
        "unknown or already recovered draft {draft_id:?}"
    );
    Ok(())
}

/// Explicitly restore a detached draft into its session composer. This is the
/// only operation that merges client-local draft state back into the legacy
/// session field, and the transaction marks the source draft recovered at the
/// same durable boundary.
pub fn recover_detached_draft(draft_id: &str) -> Result<String> {
    let draft_id = draft_id.to_owned();
    submit_database_write("recover_detached_draft", move |_| {
        recover_detached_draft_at(&database_path(), &draft_id)
    })
}

pub(super) fn recover_detached_draft_at(path: &Path, draft_id: &str) -> Result<String> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (session_id, text): (Option<String>, String) = tx
        .query_row(
            "SELECT session_id, text FROM detached_drafts
              WHERE draft_id = ?1 AND recovered_at IS NULL",
            [draft_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .with_context(|| format!("find recoverable draft {draft_id:?}"))?;
    let session_id = session_id.context("draft is not associated with a session")?;
    let changed = tx.execute(
        "UPDATE sessions SET draft_input = ?2 WHERE session_id = ?1",
        params![session_id, text],
    )?;
    ensure!(changed == 1, "draft session no longer exists");
    let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    tx.execute(
        "UPDATE detached_drafts SET recovered_at = ?2 WHERE draft_id = ?1",
        params![draft_id, now],
    )?;
    tx.commit()?;
    Ok(session_id)
}

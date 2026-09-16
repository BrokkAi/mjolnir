use super::*;

/// Persist one operational session without rewriting unrelated controller
/// state. Dashboard lifecycle jobs use this path so independent jobs can
/// commit concurrently without restoring stale copies of other sessions.
pub fn save_session(session: &SessionRecord) -> Result<()> {
    let session = session.clone();
    submit_database_write("save_session", move |_| {
        save_session_to(&database_path(), &session)
    })
}

/// Persist a borrowed-target child and its parent relationship atomically.
pub fn save_subagent_session(
    session: &SessionRecord,
    subagent: &mj_core::subagent::SubagentRecord,
) -> Result<()> {
    let session = session.clone();
    let subagent = subagent.clone();
    submit_database_write("save_subagent_session", move |_| {
        let mut connection = open(&database_path())?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        insert_session(&tx, &session)?;
        tx.execute(
            "INSERT INTO subagent_sessions(
                 child_session_id, parent_session_id, request_key, record_json
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                subagent.child_session_id,
                subagent.parent_session_id,
                subagent.request_key,
                serde_json::to_string(&subagent)?,
            ],
        )?;
        tx.commit()?;
        Ok(())
    })
}

/// Record the child turn whose completion notice the parent already has.
pub fn mark_subagent_turn_noticed(child_session_id: &str, turn: u64) -> Result<()> {
    let child_session_id = child_session_id.to_owned();
    submit_database_write("mark_subagent_turn_noticed", move |_| {
        let mut relation = load_subagent(&child_session_id)?
            .with_context(|| format!("unknown sub-agent session {child_session_id}"))?;
        relation.noticed_turn = Some(turn);
        let json = serde_json::to_string(&relation)?;
        let connection = open(&database_path())?;
        connection.execute(
            "UPDATE subagent_sessions SET record_json = ?2 WHERE child_session_id = ?1",
            params![child_session_id, json],
        )?;
        Ok(())
    })
}

pub fn load_subagent(child_session_id: &str) -> Result<Option<mj_core::subagent::SubagentRecord>> {
    let connection = open_reader(&database_path())?;
    connection
        .query_row(
            "SELECT record_json FROM subagent_sessions WHERE child_session_id = ?1",
            [child_session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|json| serde_json::from_str(&json).context("decode sub-agent record"))
        .transpose()
}

pub fn list_subagents(parent_session_id: &str) -> Result<Vec<mj_core::subagent::SubagentRecord>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT record_json FROM subagent_sessions
         WHERE parent_session_id = ?1 ORDER BY rowid",
    )?;
    statement
        .query_map([parent_session_id], |row| row.get::<_, String>(0))?
        .map(|row| serde_json::from_str(&row?).context("decode sub-agent record"))
        .collect()
}

pub fn lookup_subagent_request(
    parent_session_id: &str,
    request_key: &str,
) -> Result<Option<mj_core::subagent::SubagentRecord>> {
    let connection = open_reader(&database_path())?;
    connection
        .query_row(
            "SELECT record_json FROM subagent_sessions
             WHERE parent_session_id = ?1 AND request_key = ?2",
            params![parent_session_id, request_key],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|json| serde_json::from_str(&json).context("decode sub-agent record"))
        .transpose()
}

/// Persist a session and the container size it most recently launched on its
/// host in one transaction.
pub fn save_session_with_container_size(
    session: &SessionRecord,
    host: &str,
    size: HostContainerSize,
) -> Result<()> {
    let session = session.clone();
    let host = host.to_owned();
    submit_database_write("save_session_with_container_size", move |_| {
        save_session_with_container_size_to(&database_path(), &session, Some((&host, size)))
    })
}

/// Update only the fields a lifecycle transition owns on a session that
/// already exists. Everything else — display titles, checkpoints, container
/// settings, and attached directories — stays with its own writer.
pub fn save_lifecycle_session(session: &SessionRecord) -> Result<()> {
    let session = session.clone();
    submit_database_write("save_lifecycle_session", move |_| {
        save_lifecycle_session_to(&database_path(), &session)
    })
}

/// Install a lifecycle transition together with the checkpoint it just
/// verified and the harness session id that produced it.
pub fn save_checkpointed_session(session: &SessionRecord) -> Result<()> {
    let session = session.clone();
    submit_database_write("save_checkpointed_session", move |_| {
        save_checkpointed_session_to(&database_path(), &session)
    })
}

/// Recover lifecycle rows stranded by a process exit during checkpoint
/// creation. This must be called once by the top-level controller process
/// while it owns the controller-store guard, not by per-operation reloads.
pub fn recover_interrupted_checkpointing_sessions(updated_at: &str) -> Result<usize> {
    let updated_at = updated_at.to_owned();
    submit_database_write("recover_interrupted_checkpointing_sessions", move |_| {
        recover_interrupted_checkpointing_sessions_to(&database_path(), &updated_at)
    })
}

/// Change only the user-owned display name. This avoids writing a stale
/// SessionRecord over independently committed checkpoint or relay metadata.
pub fn set_session_title_override(session_id: &str, title: &str, updated_at: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let title = title.to_owned();
    let updated_at = updated_at.to_owned();
    submit_database_write("set_session_title_override", move |_| {
        set_session_title_override_to(&database_path(), &session_id, &title, &updated_at)
    })
}

/// Rewrite a configured profile id in every persisted session in one SQLite
/// transaction. Configuration is stored separately, so the controller owns
/// coordinating this update with the matching config-map rename.
pub fn rename_profile_references(old_id: &str, new_id: &str) -> Result<usize> {
    rename_session_reference("last_profile", old_id, new_id)
}

/// Rewrite a configured target id in every persisted session in one SQLite
/// transaction.
pub fn rename_target_references(old_id: &str, new_id: &str) -> Result<usize> {
    rename_session_reference("target_template_id", old_id, new_id)
}

pub(super) fn rename_session_reference(
    column: &'static str,
    old_id: &str,
    new_id: &str,
) -> Result<usize> {
    ensure!(
        matches!(column, "last_profile" | "target_template_id"),
        "unsupported session reference column"
    );
    let old_id = old_id.to_owned();
    let new_id = new_id.to_owned();
    submit_database_write("rename_session_reference", move |_| {
        rename_session_reference_at(&database_path(), column, &old_id, &new_id)
    })
}

pub(super) fn rename_session_reference_at(
    path: &Path,
    column: &str,
    old_id: &str,
    new_id: &str,
) -> Result<usize> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        &format!("UPDATE sessions SET {column} = ?2 WHERE {column} = ?1"),
        params![old_id, new_id],
    )?;
    tx.commit()?;
    Ok(changed)
}

/// Change only whether the resume dialog hides this session. Archiving is a
/// display choice, so it has its own writer and never rewrites lifecycle,
/// checkpoint, or title columns another task owns.
pub fn set_session_archived(session_id: &str, archived: bool) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("set_session_archived", move |_| {
        set_session_archived_to(&database_path(), &session_id, archived)
    })
}

/// Record that the managed target of an otherwise live session is definitively
/// gone. A verified checkpoint keeps the session recoverable as an error on the
/// dashboard; without one, the session is lost. The state predicate keeps a
/// late poll result from overwriting a concurrent lifecycle transition.
pub fn mark_session_target_missing(
    session_id: &str,
    detail: &str,
    updated_at: &str,
) -> Result<Option<SessionState>> {
    let session_id = session_id.to_owned();
    let detail = detail.to_owned();
    let updated_at = updated_at.to_owned();
    submit_database_write("mark_session_target_missing", move |_| {
        mark_session_target_missing_to(&database_path(), &session_id, &detail, &updated_at)
    })
}

pub(super) fn mark_session_target_missing_to(
    path: &Path,
    session_id: &str,
    detail: &str,
    updated_at: &str,
) -> Result<Option<SessionState>> {
    mark_session_target_missing_if_current_to(path, session_id, detail, updated_at, None)
}

/// Record a definitive worker failure only while the observed session record
/// is still current. A delayed background write must not invalidate a resume.
pub fn mark_session_target_missing_if_current(
    session_id: &str,
    detail: &str,
    updated_at: &str,
    observed_updated_at: &str,
) -> Result<Option<SessionState>> {
    let session_id = session_id.to_owned();
    let detail = detail.to_owned();
    let updated_at = updated_at.to_owned();
    let observed_updated_at = observed_updated_at.to_owned();
    submit_database_write("mark_session_target_missing_if_current", move |_| {
        mark_session_target_missing_if_current_to(
            &database_path(),
            &session_id,
            &detail,
            &updated_at,
            Some(&observed_updated_at),
        )
    })
}

pub(super) fn mark_session_target_missing_if_current_to(
    path: &Path,
    session_id: &str,
    detail: &str,
    updated_at: &str,
    observed_updated_at: Option<&str>,
) -> Result<Option<SessionState>> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE sessions
         SET state = CASE
                 WHEN EXISTS(
                     SELECT 1 FROM session_checkpoints
                     WHERE session_checkpoints.session_id = sessions.session_id
                 ) THEN 'error'
                 ELSE 'lost'
             END,
             last_error = ?2,
             updated_at = ?3
         WHERE session_id = ?1
           AND (?4 IS NULL OR updated_at = ?4)
           AND state IN ('provisioning', 'running', 'disconnected', 'error')",
        params![session_id, detail, updated_at, observed_updated_at],
    )?;
    ensure!(changed <= 1, "updated {changed} sessions for {session_id}");
    let state = if changed == 1 {
        let stored: String = tx.query_row(
            "SELECT state FROM sessions WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        Some(stored_session_state(&stored))
    } else {
        None
    };
    tx.commit()?;
    Ok(state)
}

pub(super) fn set_session_archived_to(path: &Path, session_id: &str, archived: bool) -> Result<()> {
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions SET archived = ?2 WHERE session_id = ?1",
        params![session_id, archived],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

/// Native sessions the resume dialog hides. Hel never writes into a harness
/// home, so the hidden set lives here instead of in the harness's own store.
pub fn hidden_native_sessions() -> Result<BTreeSet<(mj_core::config::HarnessKind, String)>> {
    hidden_native_sessions_from(&database_path())
}

pub(super) fn hidden_native_sessions_from(
    path: &Path,
) -> Result<BTreeSet<(mj_core::config::HarnessKind, String)>> {
    let connection = open_reader(path)?;
    let mut statement =
        connection.prepare("SELECT harness_kind, native_session_id FROM hidden_native_sessions")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut hidden = BTreeSet::new();
    for row in rows {
        let (harness, native_session_id) = row?;
        // Rows for a harness this release no longer supports are ignored, not
        // fatal; they simply hide nothing.
        match harness.parse::<mj_core::config::HarnessKind>() {
            Ok(harness) => {
                hidden.insert((harness, native_session_id));
            }
            Err(_) => tracing::warn!(
                harness = %harness,
                "ignoring a hidden native session for a harness that is no longer supported"
            ),
        }
    }
    Ok(hidden)
}

/// Hide or reveal one native session in the resume dialog.
pub fn set_native_session_hidden(
    harness: mj_core::config::HarnessKind,
    native_session_id: &str,
    hidden: bool,
) -> Result<()> {
    let native_session_id = native_session_id.to_owned();
    submit_database_write("set_native_session_hidden", move |_| {
        set_native_session_hidden_to(&database_path(), harness, &native_session_id, hidden)
    })
}

pub(super) fn set_native_session_hidden_to(
    path: &Path,
    harness: mj_core::config::HarnessKind,
    native_session_id: &str,
    hidden: bool,
) -> Result<()> {
    if native_session_id.trim().is_empty() {
        bail!("native session id is empty");
    }
    let connection = open(path)?;
    if hidden {
        connection.execute(
            "INSERT INTO hidden_native_sessions(harness_kind, native_session_id, hidden_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(harness_kind, native_session_id) DO NOTHING",
            params![harness.id(), native_session_id, Utc::now().to_rfc3339()],
        )?;
    } else {
        connection.execute(
            "DELETE FROM hidden_native_sessions
             WHERE harness_kind = ?1 AND native_session_id = ?2",
            params![harness.id(), native_session_id],
        )?;
    }
    Ok(())
}

/// Change only the per-session container provisioning inputs: the size
/// overrides and the attached directories. Everything else the session row
/// owns is left to its own writer.
pub fn set_session_container_settings(
    session_id: &str,
    cpus: Option<&str>,
    memory: Option<&str>,
    mounts: &[AdditionalMount],
    updated_at: &str,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let cpus = cpus.map(str::to_owned);
    let memory = memory.map(str::to_owned);
    let mounts = mounts.to_vec();
    let updated_at = updated_at.to_owned();
    submit_database_write("set_session_container_settings", move |_| {
        set_session_container_settings_to(
            &database_path(),
            &session_id,
            cpus.as_deref(),
            memory.as_deref(),
            &mounts,
            &updated_at,
        )
    })
}

pub(super) fn set_session_container_settings_to(
    path: &Path,
    session_id: &str,
    cpus: Option<&str>,
    memory: Option<&str>,
    mounts: &[AdditionalMount],
    updated_at: &str,
) -> Result<()> {
    if updated_at.trim().is_empty() {
        bail!("session update timestamp is empty");
    }
    crate::targets::validate_additional_mounts(mounts)?;
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE sessions
         SET container_cpus = ?2, container_memory = ?3, updated_at = ?4
         WHERE session_id = ?1",
        params![session_id, cpus, memory, updated_at],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    tx.execute(
        "DELETE FROM session_mounts WHERE session_id = ?1",
        [session_id],
    )?;
    for (ordinal, mount) in mounts.iter().enumerate() {
        tx.execute(
            "INSERT INTO session_mounts(session_id, ordinal, source, destination, read_only)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session_id,
                ordinal as i64,
                path_to_blob(&mount.source),
                path_to_blob(&mount.destination),
                mount.read_only
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn set_session_title_override_to(
    path: &Path,
    session_id: &str,
    title: &str,
    updated_at: &str,
) -> Result<()> {
    if title.trim().is_empty() {
        bail!("session title is empty");
    }
    if updated_at.trim().is_empty() {
        bail!("session update timestamp is empty");
    }
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions
         SET session_title_override = ?2, updated_at = ?3
         WHERE session_id = ?1",
        params![session_id, title, updated_at],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

/// Persist the latest ACP-provided title without replacing unrelated session
/// fields that may have changed in another supervised controller task.
pub fn set_session_acp_title(session_id: &str, title: Option<&str>) -> Result<()> {
    let session_id = session_id.to_owned();
    let title = title.map(str::to_owned);
    submit_database_write("set_session_acp_title", move |_| {
        set_session_acp_title_to(&database_path(), &session_id, title.as_deref())
    })
}

pub(super) fn set_session_acp_title_to(
    path: &Path,
    session_id: &str,
    title: Option<&str>,
) -> Result<()> {
    if title.is_some_and(|title| title.trim().is_empty()) {
        bail!("ACP session title is empty");
    }
    let title = title.and_then(mj_core::state::normalize_session_title);
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions SET acp_session_title = ?2 WHERE session_id = ?1",
        params![session_id, title],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

/// Commit the successful handshake for a newly provisioned worker without
/// replacing checkpoint or display metadata owned by other controller tasks.
pub fn mark_session_worker_connected(
    session_id: &str,
    native_session_id: Option<&str>,
    updated_at: &str,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let native_session_id = native_session_id.map(str::to_owned);
    let updated_at = updated_at.to_owned();
    submit_database_write("mark_session_worker_connected", move |_| {
        mark_session_worker_connected_to(
            &database_path(),
            &session_id,
            native_session_id.as_deref(),
            &updated_at,
        )
    })
}

/// Point a session at a native session its worker opened on its own. Only that
/// column moves: the session's lifecycle state belongs to whatever operation is
/// running.
pub fn adopt_native_session_id(session_id: &str, native_session_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let native_session_id = native_session_id.to_owned();
    submit_database_write("adopt_native_session_id", move |_| {
        adopt_native_session_id_to(&database_path(), &session_id, &native_session_id)
    })
}

pub(super) fn adopt_native_session_id_to(
    path: &Path,
    session_id: &str,
    native_session_id: &str,
) -> Result<()> {
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions SET native_session_id = ?2 WHERE session_id = ?1",
        params![session_id, native_session_id],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

pub(super) fn mark_session_worker_connected_to(
    path: &Path,
    session_id: &str,
    native_session_id: Option<&str>,
    updated_at: &str,
) -> Result<()> {
    if updated_at.trim().is_empty() {
        bail!("worker connection timestamp is empty");
    }
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions
         SET state = 'running',
             native_session_id = coalesce(?2, native_session_id),
             updated_at = ?3,
             last_error = NULL
         WHERE session_id = ?1",
        params![session_id, native_session_id, updated_at],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

pub(super) fn recover_interrupted_checkpointing_sessions_to(
    path: &Path,
    updated_at: &str,
) -> Result<usize> {
    if updated_at.trim().is_empty() {
        bail!("checkpoint recovery timestamp is empty");
    }
    let connection = open(path)?;
    connection
        .execute(
            "UPDATE sessions
             SET state = 'running', updated_at = ?1, last_checkpoint_error = ?2
             WHERE state = 'checkpointing'",
            params![
                updated_at,
                "checkpointing was interrupted by a controller restart; the target was left running"
            ],
        )
        .context("recover interrupted checkpointing sessions")
}

pub(super) fn save_session_to(path: &Path, session: &SessionRecord) -> Result<()> {
    save_session_with_container_size_to(path, session, None)
}

pub(super) fn save_session_with_container_size_to(
    path: &Path,
    session: &SessionRecord,
    container_size: Option<(&str, HostContainerSize)>,
) -> Result<()> {
    validate_session_record(session)?;

    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    if let Some(existing_bundle) = tx
        .query_row(
            "SELECT bundle_id FROM session_contexts WHERE session_id = ?1",
            [session.id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        && existing_bundle != session.bundle_id
    {
        bail!(
            "session {} was already associated with bundle {}, not {}",
            session.id,
            existing_bundle,
            session.bundle_id
        );
    }
    let mut session = session.clone();
    let moving: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_moves WHERE session_id=?1
         AND json_extract(operation_json, '$.phase') IN ('preparing','closing_source','resuming_destination','starting_queue'))",
        [&session.id], |row| row.get(0),
    )?;
    if moving {
        // A Move may provision for minutes while clients keep editing drafts
        // and titles. Merge these independently owned fields in this same
        // transaction rather than restoring the lifecycle's earlier copy.
        let (draft, title, acp_title, viewed, archived) = tx.query_row(
            "SELECT draft_input, session_title_override, acp_session_title, viewed_through_event_ordinal, archived
             FROM sessions WHERE session_id=?1", [&session.id], |row| Ok((
                row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, Option<String>>(2)?,
                row.get::<_, u64>(3)?, row.get::<_, bool>(4)?,
            )),
        )?;
        session.draft_input = draft;
        session.session_title_override = title;
        session.acp_session_title = acp_title;
        session.viewed_through_event_ordinal = viewed;
        session.archived = archived;
    }
    insert_session(&tx, &session)?;
    if let Some((host, size)) = container_size {
        write_host_container_size(&tx, host, size)?;
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn save_lifecycle_session_to(path: &Path, session: &SessionRecord) -> Result<()> {
    validate_session_record(session)?;

    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    update_lifecycle_fields(&tx, session)?;
    tx.commit()?;
    Ok(())
}

pub(super) fn save_checkpointed_session_to(path: &Path, session: &SessionRecord) -> Result<()> {
    validate_session_record(session)?;

    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    update_lifecycle_fields(&tx, session)?;
    tx.execute(
        "UPDATE sessions SET native_session_id = ?2 WHERE session_id = ?1",
        params![session.id, session.native_session_id],
    )?;
    replace_checkpoint(&tx, session)?;
    tx.commit()?;
    Ok(())
}

pub(super) fn validate_session_record(session: &SessionRecord) -> Result<()> {
    let mut validation = State::default();
    validation
        .sessions
        .insert(session.id.clone(), session.clone());
    validation.validate()
}

/// Remove one operational session while retaining its relational history
/// context and prompt history.
pub fn delete_session(session_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("delete_session", move |_| {
        delete_session_from(&database_path(), &session_id)
    })
}

pub(super) fn delete_session_from(path: &Path, session_id: &str) -> Result<()> {
    let connection = open(path)?;
    connection.execute("DELETE FROM sessions WHERE session_id = ?1", [session_id])?;
    Ok(())
}

/// Overwrite the unsent chat input carried across a detach. Unlike the read
/// receipt this is not monotonic: a draft can shrink, and an empty string
/// clears it.
pub fn set_session_draft_input(session_id: &str, draft: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let draft = draft.to_owned();
    submit_database_write("set_session_draft_input", move |_| {
        set_session_draft_input_at(&database_path(), &session_id, &draft)
    })
}

pub(super) fn set_session_draft_input_at(path: &Path, session_id: &str, draft: &str) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let updated = tx.execute(
        "UPDATE sessions SET draft_input = ?2 WHERE session_id = ?1",
        params![session_id, draft],
    )?;
    ensure!(updated == 1, "unknown session {session_id}");
    tx.commit()?;
    Ok(())
}

/// Retire a submitted shared draft without erasing a newer client's edit.
pub fn clear_session_draft_input_if_matches(session_id: &str, expected: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let expected = expected.to_owned();
    submit_database_write("clear_session_draft_input_if_matches", move |connection| {
        connection.execute(
            "UPDATE sessions SET draft_input = '' WHERE session_id = ?1 AND draft_input = ?2",
            params![session_id, expected],
        )?;
        Ok(())
    })
}

pub fn record_recovery_success(
    session_id: &str,
    native_session_id: &str,
    checkpoint: &CheckpointMetadata,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let native_session_id = native_session_id.to_owned();
    let checkpoint = checkpoint.clone();
    submit_database_write("record_recovery_success", move |_| {
        record_recovery_success_to(
            &database_path(),
            &session_id,
            &native_session_id,
            &checkpoint,
        )
    })
}

pub(super) fn record_recovery_success_to(
    path: &Path,
    session_id: &str,
    native_session_id: &str,
    checkpoint: &CheckpointMetadata,
) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE sessions
         SET native_session_id = ?2, last_checkpoint_error = NULL
         WHERE session_id = ?1",
        params![session_id, native_session_id],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    tx.execute(
        "INSERT INTO session_checkpoints(
             session_id, archive_path, sha256, created_at, event_frontier
         ) VALUES (?1,?2,?3,?4,?5)
         ON CONFLICT(session_id) DO UPDATE SET
             archive_path = excluded.archive_path,
             sha256 = excluded.sha256,
             created_at = excluded.created_at,
             event_frontier = excluded.event_frontier",
        params![
            session_id,
            path_to_blob(&checkpoint.archive_path),
            checkpoint.sha256,
            checkpoint.created_at,
            checkpoint.event_frontier,
        ],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn record_recovery_failure(session_id: &str, detail: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let detail = detail.to_owned();
    submit_database_write("record_recovery_failure", move |_| {
        record_recovery_failure_to(&database_path(), &session_id, &detail)
    })
}

pub(super) fn record_recovery_failure_to(
    path: &Path,
    session_id: &str,
    detail: &str,
) -> Result<()> {
    let connection = open(path)?;
    let changed = connection.execute(
        "UPDATE sessions SET last_checkpoint_error = ?2 WHERE session_id = ?1",
        params![session_id, detail],
    )?;
    if changed != 1 {
        bail!("unknown session {session_id}");
    }
    Ok(())
}

/// Re-associate a session with another project bundle.
///
/// A session's bundle is otherwise fixed, because prompt history is grouped by
/// it. Resume calls this when it converts a session between its raw and bundle
/// representations: the project is the same, so its history follows it, and
/// only the name Hel files it under changes.
pub fn rebind_session_bundle(session_id: &str, bundle_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let bundle_id = bundle_id.to_owned();
    submit_database_write("rebind_session_bundle", move |_| {
        rebind_session_bundle_to(&database_path(), &session_id, &bundle_id)
    })
}

pub(super) fn rebind_session_bundle_to(
    path: &Path,
    session_id: &str,
    bundle_id: &str,
) -> Result<()> {
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute(
        "UPDATE session_contexts SET bundle_id = ?2 WHERE session_id = ?1",
        params![session_id, bundle_id],
    )?;
    if changed == 0 {
        tx.execute(
            "INSERT INTO session_contexts(session_id, bundle_id, created_at) VALUES (?1, ?2, ?3)",
            params![session_id, bundle_id, Utc::now().to_rfc3339()],
        )?;
    }
    tx.commit()?;
    Ok(())
}

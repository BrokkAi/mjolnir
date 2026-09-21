use super::*;

pub fn load_state() -> Result<State> {
    load_state_from(&database_path())
}

pub fn load_state_from(path: &Path) -> Result<State> {
    let mut reader = open_reader(path)?;
    // Sessions and their relationships must come from the same WAL snapshot.
    // Otherwise a concurrent spawn can look orphaned despite intact foreign keys.
    let connection = reader.transaction()?;
    let mut state = State::default();
    let mut statement = connection.prepare(
        "SELECT s.session_id, s.title, s.harness_kind, s.last_profile, c.bundle_id,
                s.target_template_id, s.state, s.native_session_id, s.acp_session_title,
                s.session_title_override, c.created_at, s.updated_at,
                s.viewed_through_event_ordinal, s.last_error, s.resource_allocation,
                s.last_checkpoint_error, s.project_directory, s.managed_worktree,
                s.draft_input, s.container_cpus, s.container_memory, s.archived
                , c.workspace_id, s.create_managed_worktree, s.mjolnir_subagents,
                s.container_workspace, s.build_cache_json
         FROM sessions s JOIN session_contexts c USING(session_id)
         ORDER BY s.session_id",
    )?;
    let rows = statement.query_map([], |row| {
        // A harness Mjolnir no longer supports can still own rows an earlier
        // release wrote. Skip such a session with a warning rather than
        // failing the whole listing and hiding every other session with it.
        let harness_text: String = row.get(2)?;
        let Ok(harness_kind) = harness_text.parse() else {
            let session_id: String = row.get(0)?;
            tracing::warn!(
                session_id,
                harness = %harness_text,
                "session harness is no longer supported; the session is not listed"
            );
            return Ok(None);
        };
        Ok(Some(SessionRecord {
            harness_kind,
            create_managed_worktree: row.get(23)?,
            mjolnir_subagents: row.get(24)?,
            container_workspace: row.get::<_, Option<String>>(25)?.map(PathBuf::from),
            build_cache: row
                .get::<_, Option<String>>(26)?
                .as_deref()
                .and_then(|text| match serde_json::from_str(text) {
                    Ok(build_cache) => Some(build_cache),
                    Err(error) => {
                        tracing::warn!(%error, "session build cache record is unreadable");
                        None
                    }
                }),
            workspace_id: row.get(22)?,
            archived: row.get(21)?,
            container_cpus: row.get(19)?,
            container_memory: row.get(20)?,
            id: row.get(0)?,
            title: row.get(1)?,
            last_profile: row.get(3)?,
            bundle_id: row.get(4)?,
            project_directory: row.get_ref(16)?.blob_or_null()?.map(blob_to_path),
            managed_worktree: row
                .get::<_, Option<String>>(17)?
                .map(|json| serde_json::from_str::<ManagedWorktree>(&json))
                .transpose()
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        17,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            target_template_id: row.get(5)?,
            resource_allocation: row
                .get::<_, Option<String>>(14)?
                .map(|json| serde_json::from_str::<SessionResourceAllocation>(&json))
                .transpose()
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        14,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            additional_mounts: Vec::new(),
            state: stored_session_state(&row.get::<_, String>(6)?),
            target: None,
            native_session_id: row.get(7)?,
            acp_session_title: row
                .get::<_, Option<String>>(8)?
                .as_deref()
                .and_then(mj_core::state::normalize_session_title),
            session_title_override: row.get(9)?,
            created_at: row.get(10)?,
            updated_at: row.get(11)?,
            viewed_through_event_ordinal: row.get::<_, u64>(12)?,
            draft_input: row.get(18)?,
            last_error: row.get(13)?,
            last_checkpoint_error: row.get(15)?,
            checkpoint: None,
        }))
    })?;
    for row in rows {
        if let Some(session) = row? {
            state.sessions.insert(session.id.clone(), session);
        }
    }
    #[cfg(test)]
    super::tests::after_state_sessions_read();
    let mut statement = connection.prepare(
        "SELECT child_session_id, record_json FROM subagent_sessions ORDER BY child_session_id",
    )?;
    let rows = statement.query_map([], |row| {
        let child_id = row.get::<_, String>(0)?;
        let json = row.get::<_, String>(1)?;
        let record = serde_json::from_str::<SubagentRecord>(&json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(1, Type::Text, Box::new(error))
        })?;
        Ok((child_id, record))
    })?;
    for row in rows {
        let (child_id, record) = row?;
        // A relation whose child or parent is not among the sessions this load
        // returned describes nothing. Keeping it would fail the state check
        // below and make every later operation fail with it, which is how a
        // sub-agent spawn came to be refused with "sub-agent ... has no child
        // session" long after the child in question was gone (#1065). The load
        // already skips a session whose harness it cannot parse; a relation
        // that pointed at such a session is the same kind of residue, and
        // `save_state_to` deletes these rows on the next save.
        let missing = if !state.sessions.contains_key(&child_id) {
            Some("child")
        } else if !state.sessions.contains_key(&record.parent_session_id) {
            Some("parent")
        } else {
            None
        };
        if let Some(missing) = missing {
            tracing::warn!(
                child_session_id = child_id,
                parent_session_id = record.parent_session_id,
                missing,
                "dropping a sub-agent relation whose session is not in this state"
            );
            continue;
        }
        state.subagents.insert(child_id, record);
    }
    load_targets(&connection, &mut state)?;
    load_mounts(&connection, &mut state)?;
    load_checkpoints(&connection, &mut state)?;
    let mut statement =
        connection.prepare("SELECT host, source FROM mount_history ORDER BY host, ordinal")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            blob_to_path(row.get_ref(1)?.as_blob()?),
        ))
    })?;
    for row in rows {
        let (host, source) = row?;
        state.mount_history.entry(host).or_default().push(source);
    }
    let mut statement = connection
        .prepare("SELECT host, cpus, memory_bytes FROM host_container_sizes ORDER BY host")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            HostContainerSize {
                cpus: row.get::<_, i64>(1)? as u64,
                memory_bytes: row.get::<_, i64>(2)? as u64,
            },
        ))
    })?;
    for row in rows {
        let (host, size) = row?;
        state.container_sizes.insert(host, size);
    }
    state.validate()?;
    Ok(state)
}

pub fn save_state(state: &State) -> Result<()> {
    let state = state.clone();
    submit_database_write("save_state", move |_| {
        save_state_to(&database_path(), &state)
    })
}

pub fn save_state_to(path: &Path, state: &State) -> Result<()> {
    state.validate()?;
    let mut connection = open(path)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let existing_contexts = existing_contexts(&tx)?;
    let existing_sessions = {
        let mut statement = tx.prepare("SELECT session_id FROM sessions")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    tx.execute(
        "DELETE FROM subagent_sessions
         WHERE child_session_id NOT IN (SELECT session_id FROM sessions)
            OR parent_session_id NOT IN (SELECT session_id FROM sessions)",
        [],
    )?;
    let existing_subagents = {
        let mut statement =
            tx.prepare("SELECT child_session_id, parent_session_id FROM subagent_sessions")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (child_id, parent_id) in existing_subagents {
        if !state.subagents.contains_key(&child_id)
            || !state.sessions.contains_key(&child_id)
            || !state.sessions.contains_key(&parent_id)
        {
            tx.execute(
                "DELETE FROM subagent_sessions WHERE child_session_id = ?1",
                [child_id],
            )?;
        }
    }
    for session_id in existing_sessions {
        if !state.sessions.contains_key(&session_id) {
            tx.execute("DELETE FROM sessions WHERE session_id = ?1", [session_id])?;
        }
    }
    tx.execute("DELETE FROM mount_history", [])?;
    tx.execute("DELETE FROM host_container_sizes", [])?;
    for session in state.sessions.values() {
        if let Some((existing_bundle, existing_workspace)) = existing_contexts.get(&session.id) {
            ensure!(
                existing_bundle == &session.bundle_id,
                "session {} was already associated with bundle {}, not {}",
                session.id,
                existing_bundle,
                session.bundle_id
            );
            ensure!(
                existing_workspace == &session.workspace_id,
                "session {} was already associated with workspace {}, not {}",
                session.id,
                existing_workspace,
                session.workspace_id
            );
        }
        insert_session(&tx, session)?;
    }
    for subagent in state.subagents.values() {
        let record_json = serde_json::to_string(subagent)?;
        tx.execute(
            "INSERT INTO subagent_sessions(
                 child_session_id, parent_session_id, request_key, record_json
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(child_session_id) DO UPDATE SET
                 parent_session_id = excluded.parent_session_id,
                 request_key = excluded.request_key,
                 record_json = excluded.record_json",
            params![
                subagent.child_session_id,
                subagent.parent_session_id,
                subagent.request_key,
                record_json
            ],
        )?;
    }
    for (host, sources) in &state.mount_history {
        for (ordinal, source) in sources.iter().enumerate() {
            tx.execute(
                "INSERT INTO mount_history(host, source, ordinal) VALUES (?1, ?2, ?3)",
                params![host, path_to_blob(source), ordinal as i64],
            )?;
        }
    }
    for (host, size) in &state.container_sizes {
        write_host_container_size(&tx, host, *size)?;
    }
    tx.commit()?;
    Ok(())
}

pub(super) fn existing_contexts(
    tx: &Transaction<'_>,
) -> Result<BTreeMap<String, (String, String)>> {
    let mut statement =
        tx.prepare("SELECT session_id, bundle_id, workspace_id FROM session_contexts")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

pub(super) fn session_exists(tx: &Transaction<'_>, session_id: &str) -> Result<bool> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM sessions WHERE session_id = ?1",
            [session_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(super) fn write_materialized_session(
    tx: &Transaction<'_>,
    materialized: &MaterializedSession,
) -> Result<()> {
    let (execution, running_started_at_ms) = materialized_execution_columns(materialized.execution);
    tx.execute(
        "INSERT INTO materialized_sessions(
             session_id, applied_event_ordinal, applied_event_digest, execution_state,
             running_started_at_ms, session_title, configuration_json, last_activity_at_ms,
             pending_elicitations_json, active_turn_json, last_turn_outcome_json
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
         ON CONFLICT(session_id) DO UPDATE SET
             applied_event_ordinal = excluded.applied_event_ordinal,
             applied_event_digest = excluded.applied_event_digest,
             execution_state = excluded.execution_state,
             running_started_at_ms = excluded.running_started_at_ms,
             session_title = excluded.session_title,
             configuration_json = excluded.configuration_json,
             last_activity_at_ms = excluded.last_activity_at_ms,
             pending_elicitations_json = excluded.pending_elicitations_json,
             active_turn_json = excluded.active_turn_json,
             last_turn_outcome_json = excluded.last_turn_outcome_json",
        params![
            materialized.session_id,
            materialized.applied_event_ordinal,
            materialized.applied_event_digest,
            execution,
            running_started_at_ms,
            materialized.session_title,
            serde_json::to_string(&materialized.configuration)?,
            materialized.last_activity_at_ms,
            serde_json::to_string(&materialized.pending_elicitations)?,
            materialized
                .active_turn
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            materialized
                .last_turn_outcome
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        ],
    )?;
    tx.execute(
        "DELETE FROM materialized_transcript_items WHERE session_id = ?1",
        [materialized.session_id.as_str()],
    )?;
    for item in &materialized.transcript {
        upsert_transcript_item(tx, &materialized.session_id, item)?;
    }
    replace_materialized_queue(tx, &materialized.session_id, &materialized.queued_prompts)?;
    Ok(())
}

pub(super) fn upsert_transcript_item(
    tx: &Transaction<'_>,
    session_id: &str,
    item: &TranscriptItem,
) -> Result<()> {
    let existing = tx
        .query_row(
            "SELECT position, latest_content_event_ordinal, created_at_ms, last_changed_at_ms
             FROM materialized_transcript_items
             WHERE session_id = ?1 AND stable_id = ?2",
            params![session_id, item.stable_id],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, Option<u64>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    if let Some((position, latest_content_event_ordinal, created_at_ms, last_changed_at_ms)) =
        existing
    {
        if position != item.position || created_at_ms != item.created_at_ms {
            return Err(ProjectionIntegrityError(format!(
                "transcript item {:?} changed immutable identity fields",
                item.stable_id
            ))
            .into());
        }
        if item.last_changed_at_ms < last_changed_at_ms {
            return Err(ProjectionIntegrityError(format!(
                "transcript item {:?} moved its changed timestamp backwards",
                item.stable_id
            ))
            .into());
        }
        if latest_content_event_ordinal.is_some_and(|existing| {
            item.latest_content_event_ordinal
                .is_none_or(|next| next < existing)
        }) {
            return Err(ProjectionIntegrityError(format!(
                "transcript item {:?} moved its latest content ordinal backwards",
                item.stable_id
            ))
            .into());
        }
        tx.execute(
            "UPDATE materialized_transcript_items
             SET latest_content_event_ordinal = ?3, last_changed_at_ms = ?4, body_json = ?5
             WHERE session_id = ?1 AND stable_id = ?2",
            params![
                session_id,
                item.stable_id,
                item.latest_content_event_ordinal,
                item.last_changed_at_ms,
                serde_json::to_string(&item.body)?,
            ],
        )?;
    } else {
        tx.execute(
            "INSERT INTO materialized_transcript_items(
                 session_id, stable_id, position, latest_content_event_ordinal,
                 created_at_ms, last_changed_at_ms, body_json
             ) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                session_id,
                item.stable_id,
                item.position,
                item.latest_content_event_ordinal,
                item.created_at_ms,
                item.last_changed_at_ms,
                serde_json::to_string(&item.body)?,
            ],
        )?;
    }
    Ok(())
}

pub(super) fn replace_materialized_queue(
    tx: &Transaction<'_>,
    session_id: &str,
    queued_prompts: &[MaterializedQueuedPrompt],
) -> Result<()> {
    let mut command_ids = BTreeSet::new();
    for prompt in queued_prompts {
        if prompt.command_id.trim().is_empty() {
            bail!("materialized prompt queue has an empty command id");
        }
        if !command_ids.insert(prompt.command_id.as_str()) {
            bail!(
                "materialized prompt queue contains duplicate command {:?}",
                prompt.command_id
            );
        }
    }
    tx.execute(
        "DELETE FROM materialized_queued_prompts WHERE session_id = ?1",
        [session_id],
    )?;
    for (ordinal, prompt) in queued_prompts.iter().enumerate() {
        tx.execute(
            "INSERT INTO materialized_queued_prompts(
                 session_id, ordinal, command_id, kind_json, content_json, queued_at_ms,
                 accepted_ordinal
             ) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                session_id,
                ordinal as i64,
                prompt.command_id,
                serde_json::to_string(&prompt.kind)?,
                serde_json::to_string(&prompt.content)?,
                prompt.queued_at_ms,
                prompt.accepted_ordinal,
            ],
        )?;
    }
    Ok(())
}

pub(super) fn materialized_execution_columns(
    execution: MaterializedExecutionState,
) -> (&'static str, Option<i64>) {
    match execution {
        MaterializedExecutionState::Idle => ("idle", None),
        MaterializedExecutionState::Running { started_at_ms } => ("running", Some(started_at_ms)),
        MaterializedExecutionState::Closing => ("closing", None),
        MaterializedExecutionState::Closed => ("closed", None),
    }
}

pub(super) fn parse_materialized_execution(
    execution: &str,
    running_started_at_ms: Option<i64>,
) -> Result<MaterializedExecutionState> {
    match (execution, running_started_at_ms) {
        ("idle", None) => Ok(MaterializedExecutionState::Idle),
        ("running", Some(started_at_ms)) => {
            Ok(MaterializedExecutionState::Running { started_at_ms })
        }
        ("closing", None) => Ok(MaterializedExecutionState::Closing),
        ("closed", None) => Ok(MaterializedExecutionState::Closed),
        _ => bail!("invalid materialized execution state {execution:?}"),
    }
}

/// Write every field of a session, including the ones other writers own.
/// Only a flow that authors the whole record — creation, import, resume, or
/// orphan adoption — may use this.
pub(super) fn insert_session(tx: &Transaction<'_>, session: &SessionRecord) -> Result<()> {
    tx.execute(
        "INSERT INTO session_contexts(session_id, bundle_id, created_at, workspace_id)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(session_id) DO NOTHING",
        params![
            session.id,
            session.bundle_id,
            session.created_at,
            session.workspace_id
        ],
    )?;
    let (stored_bundle, stored_workspace): (String, String) = tx.query_row(
        "SELECT bundle_id, workspace_id FROM session_contexts WHERE session_id = ?1",
        [session.id.as_str()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        stored_bundle == session.bundle_id,
        "session {} belongs to bundle {}, not {}",
        session.id,
        stored_bundle,
        session.bundle_id
    );
    ensure!(
        stored_workspace == session.workspace_id,
        "session {} belongs to workspace {}, not {}",
        session.id,
        stored_workspace,
        session.workspace_id
    );
    tx.execute(
        "INSERT INTO sessions(
             session_id, title, harness_kind, last_profile, target_template_id, state,
             native_session_id, acp_session_title, session_title_override, updated_at,
             viewed_through_event_ordinal, last_error, resource_allocation,
             last_checkpoint_error, project_directory, managed_worktree,
             container_cpus, container_memory, archived, draft_input, create_managed_worktree,
             mjolnir_subagents, container_workspace, build_cache_json
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)
         ON CONFLICT(session_id) DO UPDATE SET
             title = excluded.title,
             harness_kind = excluded.harness_kind,
             last_profile = excluded.last_profile,
             target_template_id = excluded.target_template_id,
             state = excluded.state,
             native_session_id = excluded.native_session_id,
             acp_session_title = excluded.acp_session_title,
             session_title_override = excluded.session_title_override,
             updated_at = excluded.updated_at,
             viewed_through_event_ordinal = max(
                 sessions.viewed_through_event_ordinal,
                 excluded.viewed_through_event_ordinal
             ),
             last_error = excluded.last_error,
             resource_allocation = excluded.resource_allocation,
             last_checkpoint_error = excluded.last_checkpoint_error,
             project_directory = excluded.project_directory,
             managed_worktree = excluded.managed_worktree,
             container_cpus = excluded.container_cpus,
             container_memory = excluded.container_memory,
             archived = excluded.archived,
             create_managed_worktree = excluded.create_managed_worktree,
             mjolnir_subagents = excluded.mjolnir_subagents,
             container_workspace = excluded.container_workspace,
             build_cache_json = excluded.build_cache_json",
        params![
            session.id,
            session.title,
            session.harness_kind.id(),
            session.last_profile,
            session.target_template_id,
            session.state.as_str(),
            session.native_session_id,
            session.acp_session_title,
            session.session_title_override,
            session.updated_at,
            session.viewed_through_event_ordinal,
            session.last_error,
            session
                .resource_allocation
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            session.last_checkpoint_error,
            session
                .project_directory
                .as_ref()
                .map(|path| path_to_blob(path)),
            session
                .managed_worktree
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            session.container_cpus,
            session.container_memory,
            session.archived,
            session.draft_input,
            session.create_managed_worktree,
            session.mjolnir_subagents,
            session
                .container_workspace
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            session
                .build_cache
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        ],
    )?;
    tx.execute(
        "INSERT INTO materialized_sessions(session_id) VALUES (?1)
         ON CONFLICT(session_id) DO NOTHING",
        [session.id.as_str()],
    )?;
    replace_targets(tx, session)?;
    replace_mounts(tx, &session.id, &session.additional_mounts)?;
    replace_checkpoint(tx, session)?;
    Ok(())
}

/// Update the columns a lifecycle transition owns, plus the target locator
/// that provisioning and teardown maintain with them. The row must exist:
/// a transition never resurrects a session another writer deleted.
pub(super) fn update_lifecycle_fields(tx: &Transaction<'_>, session: &SessionRecord) -> Result<()> {
    // Destructured exhaustively and without `..` on purpose. This statement is
    // the only thing standing between a new `SessionRecord` field and a value
    // that is set in memory, read back as its default, and never missed until
    // somebody inspects the database. A new field breaks this binding, and
    // whoever adds it decides then whether a lifecycle transition owns it.
    // Everything bound to `_` is owned by `upsert_session` instead.
    let SessionRecord {
        id,
        title,
        harness_kind,
        last_profile,
        target_template_id,
        state,
        updated_at,
        viewed_through_event_ordinal,
        last_error,
        resource_allocation,
        last_checkpoint_error,
        project_directory,
        managed_worktree,
        build_cache,
        workspace_id: _,
        bundle_id: _,
        create_managed_worktree: _,
        mjolnir_subagents: _,
        additional_mounts: _,
        container_cpus: _,
        container_memory: _,
        container_workspace: _,
        archived: _,
        // Written by `replace_targets` below rather than by this statement.
        target: _,
        native_session_id: _,
        acp_session_title: _,
        session_title_override: _,
        created_at: _,
        draft_input: _,
        // Written by `replace_checkpoint`.
        checkpoint: _,
    } = session;
    let changed = tx.execute(
        // The detach ordinal only ever moves forward, so a transition that
        // started before a detach receipt cannot rewind it.
        "UPDATE sessions
         SET title = ?2,
             harness_kind = ?3,
             last_profile = ?4,
             target_template_id = ?5,
             state = ?6,
             updated_at = ?7,
             viewed_through_event_ordinal = max(viewed_through_event_ordinal, ?8),
             last_error = ?9,
             resource_allocation = ?10,
             last_checkpoint_error = ?11,
             project_directory = ?12,
             managed_worktree = ?13,
             build_cache_json = ?14
         WHERE session_id = ?1",
        params![
            id,
            title,
            harness_kind.id(),
            last_profile,
            target_template_id,
            state.as_str(),
            updated_at,
            viewed_through_event_ordinal,
            last_error,
            resource_allocation
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            last_checkpoint_error,
            project_directory.as_ref().map(|path| path_to_blob(path)),
            managed_worktree
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            // Resolved while a session is provisioned and assigned to the
            // record right before this write, so the lifecycle path owns it.
            build_cache
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        ],
    )?;
    if changed != 1 {
        bail!("unknown session {id}");
    }
    replace_targets(tx, session)
}

pub(super) fn replace_targets(tx: &Transaction<'_>, session: &SessionRecord) -> Result<()> {
    tx.execute(
        "DELETE FROM session_targets WHERE session_id = ?1",
        [session.id.as_str()],
    )?;
    if let Some(target) = &session.target {
        insert_target(tx, &session.id, target)?;
    }
    Ok(())
}

pub(super) fn replace_checkpoint(tx: &Transaction<'_>, session: &SessionRecord) -> Result<()> {
    tx.execute(
        "DELETE FROM session_checkpoints WHERE session_id = ?1",
        [session.id.as_str()],
    )?;
    if let Some(checkpoint) = &session.checkpoint {
        tx.execute(
            "INSERT INTO session_checkpoints(session_id, archive_path, sha256, created_at, event_frontier)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session.id,
                path_to_blob(&checkpoint.archive_path),
                checkpoint.sha256,
                checkpoint.created_at,
                checkpoint.event_frontier,
            ],
        )?;
    }
    Ok(())
}

pub(super) fn insert_target(
    tx: &Transaction<'_>,
    session_id: &str,
    target: &TargetLocator,
) -> Result<()> {
    let (kind, host, resource, address, workspace, worker_id, workspace_storage, borrowed_from) =
        match target {
            TargetLocator::LocalBare { worker_root } => (
                "local-bare",
                None,
                None,
                None,
                Some(path_to_blob(worker_root)),
                None,
                None,
                None,
            ),
            TargetLocator::LocalPodman {
                container_id,
                workspace_storage,
                borrowed_from,
            } => (
                "local-podman",
                None,
                Some(container_id.as_str()),
                None,
                None,
                None,
                Some(serde_json::to_string(workspace_storage)?),
                borrowed_from.as_deref(),
            ),
            TargetLocator::LocalDocker {
                container_id,
                borrowed_from,
            } => (
                "local-docker",
                None,
                Some(container_id.as_str()),
                None,
                None,
                None,
                None,
                borrowed_from.as_deref(),
            ),
            TargetLocator::SshDocker {
                host,
                container_id,
                borrowed_from,
            } => (
                "ssh-docker",
                Some(host.as_str()),
                Some(container_id.as_str()),
                None,
                None,
                None,
                None,
                borrowed_from.as_deref(),
            ),
            TargetLocator::AppleContainer {
                container_id,
                borrowed_from,
            } => (
                "apple-container",
                None,
                Some(container_id.as_str()),
                None,
                None,
                None,
                None,
                borrowed_from.as_deref(),
            ),
            TargetLocator::AwsEc2 {
                instance_id,
                address,
            } => (
                "aws-ec2",
                None,
                Some(instance_id.as_str()),
                address.as_deref(),
                None,
                None,
                None,
                None,
            ),
            TargetLocator::SshBare {
                host,
                workspace,
                worker_id,
            } => (
                "ssh-bare",
                Some(host.as_str()),
                None,
                None,
                Some(path_to_blob(workspace)),
                worker_id.as_deref(),
                None,
                None,
            ),
            TargetLocator::SshPodman {
                host,
                container_id,
                workspace_storage,
                borrowed_from,
            } => (
                "ssh-podman",
                Some(host.as_str()),
                Some(container_id.as_str()),
                None,
                None,
                None,
                Some(serde_json::to_string(workspace_storage)?),
                borrowed_from.as_deref(),
            ),
        };
    tx.execute(
        "INSERT INTO session_targets(session_id, kind, host, resource_id, address, workspace, worker_id, workspace_storage, borrowed_from)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![
            session_id,
            kind,
            host,
            resource,
            address,
            workspace,
            worker_id,
            workspace_storage,
            borrowed_from
        ],
    )?;
    Ok(())
}

pub(super) fn load_targets(connection: &Connection, state: &mut State) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT session_id, kind, host, resource_id, address, workspace, worker_id, workspace_storage, borrowed_from
         FROM session_targets",
    )?;
    let rows = statement.query_map([], |row| {
        let session_id: String = row.get(0)?;
        let kind: String = row.get(1)?;
        let host: Option<String> = row.get(2)?;
        let resource: Option<String> = row.get(3)?;
        let address: Option<String> = row.get(4)?;
        let workspace = row.get_ref(5)?.blob_or_null()?.map(blob_to_path);
        let worker_id: Option<String> = row.get(6)?;
        let workspace_storage = row
            .get::<_, Option<String>>(7)?
            .map(|serialized| {
                serde_json::from_str(&serialized).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(7, Type::Text, Box::new(error))
                })
            })
            .transpose()?
            .unwrap_or_default();
        let borrowed_from: Option<String> = row.get(8)?;
        let target = match kind.as_str() {
            "local-bare" => TargetLocator::LocalBare {
                worker_root: workspace.unwrap(),
            },
            "local-podman" => TargetLocator::LocalPodman {
                borrowed_from,
                container_id: resource.unwrap(),
                workspace_storage,
            },
            "local-docker" => TargetLocator::LocalDocker {
                borrowed_from,
                container_id: resource.unwrap(),
            },
            "apple-container" => TargetLocator::AppleContainer {
                borrowed_from,
                container_id: resource.unwrap(),
            },
            "aws-ec2" => TargetLocator::AwsEc2 {
                instance_id: resource.unwrap(),
                address,
            },
            "ssh-bare" => TargetLocator::SshBare {
                host: host.unwrap(),
                workspace: workspace.unwrap(),
                worker_id,
            },
            "ssh-docker" => TargetLocator::SshDocker {
                borrowed_from,
                host: host.unwrap(),
                container_id: resource.unwrap(),
            },
            "ssh-podman" => TargetLocator::SshPodman {
                borrowed_from,
                host: host.unwrap(),
                container_id: resource.unwrap(),
                workspace_storage,
            },
            _ => unreachable!("target kind constrained by schema"),
        };
        Ok((session_id, target))
    })?;
    for row in rows {
        let (session_id, target) = row?;
        // A session skipped for an unsupported harness has no entry to attach
        // its target, mounts, or checkpoint to.
        if let Some(session) = state.sessions.get_mut(&session_id) {
            session.target = Some(target);
        }
    }
    Ok(())
}

/// Rewrite a session's attached directories.
///
/// `session_mounts.read_only` keeps the meaning older builds understand, so
/// read-write mounts are stored there as not read-only and recorded again in
/// `session_mount_access`. Older builds rewrite `session_mounts` without
/// touching that table, which is what keeps the read-write choice.
pub(super) fn replace_mounts(
    tx: &rusqlite::Transaction<'_>,
    session_id: &str,
    mounts: &[AdditionalMount],
) -> Result<()> {
    tx.execute(
        "DELETE FROM session_mounts WHERE session_id = ?1",
        [session_id],
    )?;
    tx.execute(
        "DELETE FROM session_mount_access WHERE session_id = ?1",
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
                mount.access == MountAccess::Ro
            ],
        )?;
        if mount.access == MountAccess::Rw {
            tx.execute(
                "INSERT INTO session_mount_access(session_id, source, destination, access)
                 VALUES (?1, ?2, ?3, 'rw')",
                params![
                    session_id,
                    path_to_blob(&mount.source),
                    path_to_blob(&mount.destination)
                ],
            )?;
        }
    }
    Ok(())
}

pub(super) fn load_mounts(connection: &Connection, state: &mut State) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT m.session_id, m.source, m.destination, m.read_only, a.access IS NOT NULL
         FROM session_mounts m
         LEFT JOIN session_mount_access a
             ON a.session_id = m.session_id
             AND a.source = m.source
             AND a.destination = m.destination
         ORDER BY m.session_id, m.ordinal",
    )?;
    let rows = statement.query_map([], |row| {
        // An older build that made the mount read-only left the access row
        // behind; its later choice wins.
        let access = match (row.get::<_, bool>(3)?, row.get::<_, bool>(4)?) {
            (true, _) => MountAccess::Ro,
            (false, true) => MountAccess::Rw,
            (false, false) => MountAccess::Cow,
        };
        Ok((
            row.get::<_, String>(0)?,
            AdditionalMount {
                source: blob_to_path(row.get_ref(1)?.as_blob()?),
                destination: blob_to_path(row.get_ref(2)?.as_blob()?),
                access,
            },
        ))
    })?;
    for row in rows {
        let (session_id, mount) = row?;
        if let Some(session) = state.sessions.get_mut(&session_id) {
            session.additional_mounts.push(mount);
        }
    }
    Ok(())
}

pub(super) fn load_checkpoints(connection: &Connection, state: &mut State) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT session_id, archive_path, sha256, created_at, event_frontier FROM session_checkpoints",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            CheckpointMetadata {
                archive_path: blob_to_path(row.get_ref(1)?.as_blob()?),
                sha256: row.get(2)?,
                created_at: row.get(3)?,
                event_frontier: row.get(4)?,
            },
        ))
    })?;
    for row in rows {
        let (session_id, checkpoint) = row?;
        if let Some(session) = state.sessions.get_mut(&session_id) {
            session.checkpoint = Some(checkpoint);
        }
    }
    Ok(())
}

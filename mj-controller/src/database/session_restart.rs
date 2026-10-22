//! Durable intent for one daemon-owned session restart.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionRestartPhase {
    Stopping,
    RestoringInPlace,
    RecreatingContainer,
    CancelRequested,
    Fallback,
}

impl SessionRestartPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stopping => "stopping",
            Self::RestoringInPlace => "restoring_in_place",
            Self::RecreatingContainer => "recreating_container",
            Self::CancelRequested => "cancel_requested",
            Self::Fallback => "fallback",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "stopping" => Ok(Self::Stopping),
            "restoring_in_place" => Ok(Self::RestoringInPlace),
            "recreating_container" => Ok(Self::RecreatingContainer),
            "cancel_requested" => Ok(Self::CancelRequested),
            "fallback" => Ok(Self::Fallback),
            _ => bail!("unknown session restart phase {value:?}"),
        }
    }

    fn can_follow(self, next: Self) -> bool {
        self == next
            || next == Self::CancelRequested
            || matches!(
                (self, next),
                (
                    Self::Stopping,
                    Self::RestoringInPlace | Self::RecreatingContainer | Self::Fallback
                ) | (Self::RestoringInPlace, Self::Fallback)
                    | (Self::RecreatingContainer, Self::RestoringInPlace)
            )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionRestartIntent {
    pub session_id: String,
    pub operation_id: String,
    pub phase: SessionRestartPhase,
    pub checkpoint_sha256: Option<String>,
    pub checkpoint_started: bool,
    pub container_recreation_acknowledged: bool,
    /// Historical automatic recreation intent. New daemons never create one;
    /// recovery retains this marker to reconcile operations accepted by older builds.
    pub automatic: bool,
    pub container_upgrade: Option<crate::targets::RetainedContainer>,
}

/// Create one intent, or return the unfinished intent the session already owns.
pub(crate) fn begin_session_restart(
    session_id: &str,
    operation_id: &str,
    cancelled: Arc<AtomicBool>,
    _container_recreation_acknowledged: bool,
) -> Result<SessionRestartIntent> {
    begin_session_restart_with(session_id, operation_id, cancelled, None)
}

pub(crate) fn begin_container_upgrade(
    session_id: &str,
    operation_id: &str,
    cancelled: Arc<AtomicBool>,
    plan: crate::targets::RetainedContainer,
) -> Result<SessionRestartIntent> {
    begin_session_restart_with(session_id, operation_id, cancelled, Some(plan))
}

fn begin_session_restart_with(
    session_id: &str,
    operation_id: &str,
    cancelled: Arc<AtomicBool>,
    container_upgrade: Option<crate::targets::RetainedContainer>,
) -> Result<SessionRestartIntent> {
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    submit_database_write("begin_session_restart", move |connection| {
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "Restart was cancelled before its durable intent was recorded"
        );
        let tx = connection.transaction()?;
        let existing = load_session_restart_with(&tx, &session_id)?;
        if let Some(intent) = existing {
            ensure!(
                intent.container_upgrade == container_upgrade,
                "another Restart or container upgrade already owns this session"
            );
            tx.commit()?;
            return Ok(intent);
        }
        state_io::load_session_with(&tx, &session_id)?
            .with_context(|| format!("unknown session {session_id}"))?;
        // A Restart never acquires replacement authority from an image or
        // settings mismatch, including an acknowledgment sent by an old client.
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "Restart was cancelled before its durable intent was recorded"
        );
        tx.execute(
            "INSERT INTO session_restart_intents(session_id, operation_id, phase, updated_at, container_upgrade_json)
             VALUES (?1, ?2, 'stopping', ?3, ?4)",
            params![session_id, operation_id, Utc::now().to_rfc3339(), container_upgrade.as_ref().map(serde_json::to_string).transpose()?],
        )?;
        tx.commit()?;
        Ok(SessionRestartIntent {
            session_id,
            operation_id,
            phase: SessionRestartPhase::Stopping,
            checkpoint_sha256: None,
            checkpoint_started: false,
            container_recreation_acknowledged: false,
            automatic: false,
            container_upgrade,
        })
    })
}

pub(crate) fn load_session_restarts() -> Result<Vec<SessionRestartIntent>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT session_id, operation_id, phase, checkpoint_sha256, checkpoint_started, container_recreation_acknowledged, automatic, container_upgrade_json
         FROM session_restart_intents ORDER BY session_id",
    )?;
    statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, bool>(4)?,
                row.get::<_, bool>(5)?,
                row.get::<_, bool>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?
        .map(|row| {
            let (
                session_id,
                operation_id,
                phase,
                checkpoint_sha256,
                checkpoint_started,
                container_recreation_acknowledged,
                automatic,
                container_upgrade_json,
            ) = row?;
            Ok(SessionRestartIntent {
                session_id,
                operation_id,
                phase: SessionRestartPhase::parse(&phase)?,
                checkpoint_sha256,
                checkpoint_started,
                container_recreation_acknowledged,
                automatic,
                container_upgrade: container_upgrade_json
                    .map(|json| serde_json::from_str(&json))
                    .transpose()?,
            })
        })
        .collect()
}

pub(crate) fn record_session_restart_checkpoint(
    session_id: &str,
    operation_id: &str,
    checkpoint_sha256: &str,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    let checkpoint_sha256 = checkpoint_sha256.to_owned();
    submit_database_write("record_session_restart_checkpoint", move |connection| {
        let changed = connection.execute(
            "UPDATE session_restart_intents SET checkpoint_sha256=?3, updated_at=?4
             WHERE session_id=?1 AND operation_id=?2",
            params![
                session_id,
                operation_id,
                checkpoint_sha256,
                Utc::now().to_rfc3339()
            ],
        )?;
        ensure!(
            changed == 1,
            "session restart ownership changed before checkpoint was recorded"
        );
        Ok(())
    })
}

pub(crate) fn mark_session_restart_checkpoint_started(
    session_id: &str,
    operation_id: &str,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    submit_database_write(
        "mark_session_restart_checkpoint_started",
        move |connection| {
            let changed = connection.execute(
                "UPDATE session_restart_intents SET checkpoint_started=1, updated_at=?3
             WHERE session_id=?1 AND operation_id=?2",
                params![session_id, operation_id, Utc::now().to_rfc3339()],
            )?;
            ensure!(
                changed == 1,
                "session restart ownership changed before checkpointing"
            );
            Ok(())
        },
    )
}

pub(crate) fn advance_session_restart(
    session_id: &str,
    operation_id: &str,
    phase: SessionRestartPhase,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    submit_database_write("advance_session_restart", move |connection| {
        let tx = connection.transaction()?;
        let current = load_session_restart_with(&tx, &session_id)?
            .context("session restart intent disappeared")?;
        ensure!(
            current.operation_id == operation_id,
            "session restart ownership changed"
        );
        ensure!(
            current.phase.can_follow(phase),
            "invalid session restart phase transition from {:?} to {:?}",
            current.phase,
            phase
        );
        if phase == SessionRestartPhase::RecreatingContainer {
            ensure!(
                current.container_upgrade.is_none(),
                "explicit replacement requires atomic idle and child admission"
            );
            let session = state_io::load_session_with(&tx, &session_id)?
                .context("session disappeared before Restart recreation")?;
            ensure!(
                current.container_recreation_acknowledged
                    && current.checkpoint_started
                    && current.checkpoint_sha256.is_some()
                    && session
                        .checkpoint
                        .as_ref()
                        .map(|checkpoint| checkpoint.sha256.as_str())
                        == current.checkpoint_sha256.as_deref(),
                "Restart recreation requires its own sealed checkpoint"
            );
            ensure!(
                matches!(
                    session.state,
                    SessionState::Closing
                        | SessionState::Destroying
                        | SessionState::Error
                        | SessionState::Stopped
                        | SessionState::Provisioning
                ),
                "Restart recreation cannot remove an unsealed live session"
            );
        }
        tx.execute(
            "UPDATE session_restart_intents SET phase=?3, updated_at=?4
             WHERE session_id=?1 AND operation_id=?2",
            params![
                session_id,
                operation_id,
                phase.as_str(),
                Utc::now().to_rfc3339()
            ],
        )?;
        tx.commit()?;
        Ok(())
    })
}

/// Retain the settings lock while cancellation unwinds, but never recover
/// accepted cancellation as another Restart after a daemon crash.
pub(crate) fn request_session_restart_cancel(session_id: &str, operation_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    submit_database_write("request_session_restart_cancel", move |connection| {
        let current = load_session_restart_with(connection, &session_id)?;
        ensure!(
            !current
                .as_ref()
                .is_some_and(|intent| intent.operation_id == operation_id
                    && intent.container_upgrade.is_some()
                    && intent.phase == SessionRestartPhase::RecreatingContainer),
            "container replacement already started and cannot be cancelled"
        );
        connection.execute("UPDATE session_restart_intents SET phase='cancel_requested', updated_at=?3 WHERE session_id=?1 AND operation_id=?2",
            params![session_id, operation_id, Utc::now().to_rfc3339()])?;
        Ok(())
    })
}

/// A cancelled Restart must not become an ordinary pending suspension on
/// startup: retain its environment and settle the recovery row atomically.
pub(crate) fn recover_cancelled_session_restart(
    session_id: &str,
    operation_id: &str,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    submit_database_write("recover_cancelled_session_restart", move |connection| {
        let tx = connection.transaction()?;
        let current = load_session_restart_with(&tx, &session_id)?;
        if current.as_ref().is_some_and(|intent| {
            intent.operation_id == operation_id
                && intent.phase == SessionRestartPhase::CancelRequested
        }) {
            if let Some(mut session) = state_io::load_session_with(&tx, &session_id)?
                && session.state == SessionState::Closing
            {
                session.state = SessionState::Error;
                session.last_error = Some("Restart was cancelled; the environment was retained. Restart or suspend to continue.".into());
                session.updated_at = Utc::now().to_rfc3339();
                state_io::update_lifecycle_fields(&tx, &session)?;
            }
            tx.execute(
                "DELETE FROM session_restart_intents WHERE session_id=?1 AND operation_id=?2",
                params![session_id, operation_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
}

pub(crate) fn finish_session_restart(session_id: &str, operation_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    submit_database_write("finish_session_restart", move |connection| {
        connection.execute(
            "DELETE FROM session_restart_intents WHERE session_id=?1 AND operation_id=?2",
            params![session_id, operation_id],
        )?;
        Ok(())
    })
}

pub(crate) fn cancel_session_restart(session_id: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    submit_database_write("cancel_session_restart", move |connection| {
        connection.execute(
            "DELETE FROM session_restart_intents WHERE session_id=?1",
            [session_id],
        )?;
        Ok(())
    })
}

fn load_session_restart_with(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<SessionRestartIntent>> {
    let row = connection
        .query_row(
            "SELECT operation_id, phase, checkpoint_sha256, checkpoint_started, container_recreation_acknowledged, automatic, container_upgrade_json
         FROM session_restart_intents WHERE session_id=?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, bool>(4)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(
            operation_id,
            phase,
            checkpoint_sha256,
            checkpoint_started,
            container_recreation_acknowledged,
            automatic,
            container_upgrade_json,
        )| {
            Ok(SessionRestartIntent {
                session_id: session_id.to_owned(),
                operation_id,
                phase: SessionRestartPhase::parse(&phase)?,
                checkpoint_sha256,
                checkpoint_started,
                container_recreation_acknowledged,
                automatic,
                container_upgrade: container_upgrade_json
                    .map(|json| serde_json::from_str(&json))
                    .transpose()?,
            })
        },
    )
    .transpose()
}

/// Persist the immutable prepared image before stopping the source.
pub(crate) fn prepare_container_upgrade(
    session_id: &str,
    operation_id: &str,
    plan: crate::targets::RetainedContainer,
) -> Result<()> {
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    submit_database_write("prepare_container_upgrade", move |connection| {
        let changed = connection.execute("UPDATE session_restart_intents SET container_upgrade_json=?3 WHERE session_id=?1 AND operation_id=?2 AND phase IN ('stopping','recreating_container') AND container_upgrade_json IS NOT NULL", params![session_id, operation_id, serde_json::to_string(&plan)?])?;
        ensure!(
            changed == 1,
            "container upgrade ownership changed during preparation"
        );
        Ok(())
    })
}

/// Child startup and replacement share the database writer's decision. A
/// parked child's stale target cannot be woken after replacement commits.
pub(super) fn ensure_container_parent_available(
    connection: &Connection,
    parent: &str,
) -> Result<()> {
    let replacing: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM session_restart_intents WHERE session_id=?1 AND phase='recreating_container' AND container_upgrade_json IS NOT NULL)", [parent], |row| row.get(0))?;
    ensure!(
        !replacing,
        "the parent container is being upgraded; retry after it reconnects"
    );
    Ok(())
}

pub(crate) fn try_commit_container_upgrade(session_id: &str, operation_id: &str) -> Result<bool> {
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    submit_database_write("commit container upgrade", move |connection| {
        let tx = connection.transaction()?;
        let intent = load_session_restart_with(&tx, &session_id)?
            .context("container upgrade intent disappeared")?;
        ensure!(
            intent.operation_id == operation_id
                && intent.container_upgrade.is_some()
                && matches!(
                    intent.phase,
                    SessionRestartPhase::Stopping | SessionRestartPhase::RecreatingContainer
                ),
            "container upgrade ownership changed or cancellation was accepted"
        );
        let children = tx.prepare("SELECT s.state,t.session_id IS NOT NULL FROM subagent_sessions c JOIN sessions s ON s.session_id=c.child_session_id LEFT JOIN session_targets t ON t.session_id=s.session_id WHERE c.parent_session_id=?1")?
            .query_map([&session_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let occupied = children.into_iter().any(|(state, has_target)| {
            let state = stored_session_state(&state);
            state != SessionState::Parked && (state.has_live_worker() || has_target)
        });
        if occupied || super::delegation::has_pending_mutating_delegations_with(&tx, &session_id)? {
            return Ok(false);
        }
        tx.execute("UPDATE session_restart_intents SET phase='recreating_container', updated_at=?3 WHERE session_id=?1 AND operation_id=?2", params![session_id, operation_id, Utc::now().to_rfc3339()])?;
        tx.commit()?;
        Ok(true)
    })
}

/// The retained-container intent is the sole authority over its environment,
/// including after an engine failure or daemon replacement.
pub(crate) fn container_upgrade_owns_target(session_id: &str) -> Result<bool> {
    let reader = open_reader(&database_path())?;
    container_upgrade_owns_target_with(&reader, session_id)
}

pub(super) fn container_upgrade_owns_target_with(
    connection: &Connection,
    session_id: &str,
) -> Result<bool> {
    Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM session_restart_intents WHERE session_id=?1 AND container_upgrade_json IS NOT NULL)", [session_id], |row| row.get(0))?)
}

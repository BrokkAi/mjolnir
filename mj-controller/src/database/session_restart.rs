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
    /// Started by an upgrade rather than by a person. An automatic Restart
    /// recreates the container and never interrupts work: a session that is
    /// working makes it stand down, including when a later daemon recovers it.
    pub automatic: bool,
}

/// Create one intent, or return the unfinished intent the session already owns.
pub(crate) fn begin_session_restart(
    session_id: &str,
    operation_id: &str,
    cancelled: Arc<AtomicBool>,
    container_recreation_acknowledged: bool,
) -> Result<SessionRestartIntent> {
    begin_session_restart_with(
        session_id,
        operation_id,
        cancelled,
        container_recreation_acknowledged,
        false,
    )
}

/// Create the intent for a container recreation an upgrade starts on its own,
/// or return the unfinished automatic intent the session already owns. `None`
/// when the session owns a Restart a person asked for: that one keeps its own
/// rules, and the automatic recreation does not run under them.
pub(crate) fn begin_automatic_session_restart(
    session_id: &str,
    operation_id: &str,
    cancelled: Arc<AtomicBool>,
) -> Result<Option<SessionRestartIntent>> {
    let intent = begin_session_restart_with(session_id, operation_id, cancelled, true, true)?;
    Ok(intent.automatic.then_some(intent))
}

fn begin_session_restart_with(
    session_id: &str,
    operation_id: &str,
    cancelled: Arc<AtomicBool>,
    container_recreation_acknowledged: bool,
    automatic: bool,
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
            tx.commit()?;
            return Ok(intent);
        }
        let session = state_io::load_session_with(&tx, &session_id)?
            .with_context(|| format!("unknown session {session_id}"))?;
        if mj_core::state::ContainerSettings::recreation_required(&session)
            && !container_recreation_acknowledged
        {
            return Err(anyhow::Error::new(mj_core::refusal::Refusal::precondition(
                "Restart must recreate this container. Files outside the recovery copy may be lost. Review the recreation warning and confirm Restart again.",
            )));
        }
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "Restart was cancelled before its durable intent was recorded"
        );
        tx.execute(
            "INSERT INTO session_restart_intents(session_id, operation_id, phase, updated_at, container_recreation_acknowledged, automatic)
             VALUES (?1, ?2, 'stopping', ?3, ?4, ?5)",
            params![session_id, operation_id, Utc::now().to_rfc3339(), container_recreation_acknowledged, automatic],
        )?;
        tx.commit()?;
        Ok(SessionRestartIntent {
            session_id,
            operation_id,
            phase: SessionRestartPhase::Stopping,
            checkpoint_sha256: None,
            checkpoint_started: false,
            container_recreation_acknowledged,
            automatic,
        })
    })
}

pub(crate) fn load_session_restarts() -> Result<Vec<SessionRestartIntent>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT session_id, operation_id, phase, checkpoint_sha256, checkpoint_started, container_recreation_acknowledged, automatic
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
            ) = row?;
            Ok(SessionRestartIntent {
                session_id,
                operation_id,
                phase: SessionRestartPhase::parse(&phase)?,
                checkpoint_sha256,
                checkpoint_started,
                container_recreation_acknowledged,
                automatic,
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
            "SELECT operation_id, phase, checkpoint_sha256, checkpoint_started, container_recreation_acknowledged, automatic
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
        )| {
            Ok(SessionRestartIntent {
                session_id: session_id.to_owned(),
                operation_id,
                phase: SessionRestartPhase::parse(&phase)?,
                checkpoint_sha256,
                checkpoint_started,
                container_recreation_acknowledged,
                automatic,
            })
        },
    )
    .transpose()
}

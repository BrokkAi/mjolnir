//! Durable intent for one daemon-owned session restart.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionRestartPhase {
    Stopping,
    RestoringInPlace,
    Fallback,
}

impl SessionRestartPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stopping => "stopping",
            Self::RestoringInPlace => "restoring_in_place",
            Self::Fallback => "fallback",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "stopping" => Ok(Self::Stopping),
            "restoring_in_place" => Ok(Self::RestoringInPlace),
            "fallback" => Ok(Self::Fallback),
            _ => bail!("unknown session restart phase {value:?}"),
        }
    }

    fn can_follow(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (Self::Stopping, Self::RestoringInPlace | Self::Fallback)
                    | (Self::RestoringInPlace, Self::Fallback)
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
}

/// Create one intent, or return the unfinished intent the session already owns.
pub(crate) fn begin_session_restart(
    session_id: &str,
    operation_id: &str,
    cancelled: Arc<AtomicBool>,
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
        ensure!(
            state_io::load_session_with(&tx, &session_id)?.is_some(),
            "unknown session {session_id}"
        );
        ensure!(
            !cancelled.load(Ordering::Acquire),
            "Restart was cancelled before its durable intent was recorded"
        );
        tx.execute(
            "INSERT INTO session_restart_intents(session_id, operation_id, phase, updated_at)
             VALUES (?1, ?2, 'stopping', ?3)",
            params![session_id, operation_id, Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(SessionRestartIntent {
            session_id,
            operation_id,
            phase: SessionRestartPhase::Stopping,
            checkpoint_sha256: None,
            checkpoint_started: false,
        })
    })
}

pub(crate) fn load_session_restarts() -> Result<Vec<SessionRestartIntent>> {
    let connection = open_reader(&database_path())?;
    let mut statement = connection.prepare(
        "SELECT session_id, operation_id, phase, checkpoint_sha256, checkpoint_started
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
            ))
        })?
        .map(|row| {
            let (session_id, operation_id, phase, checkpoint_sha256, checkpoint_started) = row?;
            Ok(SessionRestartIntent {
                session_id,
                operation_id,
                phase: SessionRestartPhase::parse(&phase)?,
                checkpoint_sha256,
                checkpoint_started,
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
        let current = load_session_restart_with(connection, &session_id)?
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
        connection.execute(
            "UPDATE session_restart_intents SET phase=?3, updated_at=?4
             WHERE session_id=?1 AND operation_id=?2",
            params![
                session_id,
                operation_id,
                phase.as_str(),
                Utc::now().to_rfc3339()
            ],
        )?;
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
            "SELECT operation_id, phase, checkpoint_sha256, checkpoint_started
         FROM session_restart_intents WHERE session_id=?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            },
        )
        .optional()?;
    row.map(
        |(operation_id, phase, checkpoint_sha256, checkpoint_started)| {
            Ok(SessionRestartIntent {
                session_id: session_id.to_owned(),
                operation_id,
                phase: SessionRestartPhase::parse(&phase)?,
                checkpoint_sha256,
                checkpoint_started,
            })
        },
    )
    .transpose()
}

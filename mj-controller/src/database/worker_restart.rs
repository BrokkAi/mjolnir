//! A detached worker boot remains recoverable after the daemon releases admission.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkerRestartIntent {
    pub operation_id: String,
    pub target: TargetLocator,
    pub desired_build: String,
}

pub(crate) fn begin_worker_restart(session_id: &str, intent: &WorkerRestartIntent) -> Result<()> {
    let _owner = crate::worker_lifecycle::require(session_id)?;
    let session_id = session_id.to_owned();
    let intent = intent.clone();
    submit_database_write("begin_worker_restart", move |connection| {
        let tx = connection.transaction()?;
        let record = state_io::load_session_with(&tx, &session_id)?;
        ensure!(
            record.is_some_and(|s| s.target.as_ref() == Some(&intent.target)),
            "worker restart target changed before admission"
        );
        let claimed = tx.execute(
            "INSERT INTO worker_restart_intents(session_id,operation_id,target_json,desired_build,phase)
             VALUES (?1,?2,?3,?4,'prepared') ON CONFLICT(session_id) DO NOTHING",
            params![session_id, intent.operation_id, serde_json::to_string(&intent.target)?, intent.desired_build],
        )?;
        ensure!(
            claimed == 1,
            "another worker replacement already owns session {session_id}"
        );
        tx.commit()?;
        Ok(())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerRestartPhase {
    Prepared,
    Swapping,
    AwaitingReadiness,
}

impl WorkerRestartPhase {
    fn transition(self) -> (&'static str, &'static str) {
        match self {
            Self::Prepared => ("prepared", "prepared"),
            Self::Swapping => ("prepared", "swapping"),
            Self::AwaitingReadiness => ("swapping", "awaiting_readiness"),
        }
    }
}

pub(crate) fn worker_restart_phase(session_id: &str) -> Result<Option<WorkerRestartPhase>> {
    let connection = open_reader(&database_path())?;
    let phase: Option<String> = connection
        .query_row(
            "SELECT phase FROM worker_restart_intents WHERE session_id=?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?;
    phase
        .map(|phase| match phase.as_str() {
            "prepared" => Ok(WorkerRestartPhase::Prepared),
            "swapping" => Ok(WorkerRestartPhase::Swapping),
            "awaiting_readiness" => Ok(WorkerRestartPhase::AwaitingReadiness),
            _ => bail!("unknown worker replacement phase {phase}"),
        })
        .transpose()
}

pub(crate) fn advance_worker_restart(
    session_id: &str,
    operation_id: &str,
    phase: WorkerRestartPhase,
) -> Result<()> {
    let _owner = crate::worker_lifecycle::require(session_id)?;
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    submit_database_write("advance_worker_restart", move |connection| {
        let (previous, next) = phase.transition();
        ensure!(connection.execute(
            "UPDATE worker_restart_intents SET phase=?3 WHERE session_id=?1 AND operation_id=?2 AND phase=?4",
            params![session_id, operation_id, next, previous],
        )? == 1, "worker restart ownership changed");
        Ok(())
    })
}

pub(crate) fn load_worker_restart(session_id: &str) -> Result<Option<WorkerRestartIntent>> {
    let connection = open_reader(&database_path())?;
    load_worker_restart_with(&connection, session_id)
}

fn load_worker_restart_with(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<WorkerRestartIntent>> {
    let row: Option<(String, String, String)> = connection.query_row(
        "SELECT operation_id,target_json,desired_build FROM worker_restart_intents WHERE session_id=?1",
        [session_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?;
    row.map(|(operation_id, target, desired_build)| {
        Ok(WorkerRestartIntent {
            operation_id,
            target: serde_json::from_str(&target)?,
            desired_build,
        })
    })
    .transpose()
}

pub(crate) fn finish_worker_restart(session_id: &str, operation_id: &str) -> Result<()> {
    let _owner = crate::worker_lifecycle::require(session_id)?;
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    submit_database_write("finish_worker_restart", move |connection| {
        finish_worker_restart_with(connection, &session_id, &operation_id)
    })
}

fn finish_worker_restart_with(
    connection: &Connection,
    session_id: &str,
    operation_id: &str,
) -> Result<()> {
    connection.execute(
        "DELETE FROM worker_restart_intents WHERE session_id=?1 AND operation_id=?2",
        params![session_id, operation_id],
    )?;
    Ok(())
}

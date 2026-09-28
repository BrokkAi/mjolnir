//! A detached worker boot remains recoverable after the daemon releases admission.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkerRestartIntent {
    pub operation_id: String,
    pub target: TargetLocator,
    pub desired_build: String,
}

pub(crate) fn begin_worker_restart(session_id: &str, intent: &WorkerRestartIntent) -> Result<()> {
    let session_id = session_id.to_owned();
    let intent = intent.clone();
    submit_database_write("begin_worker_restart", move |connection| {
        let tx = connection.transaction()?;
        let record = state_io::load_session_with(&tx, &session_id)?;
        ensure!(
            record.is_some_and(|s| s.target.as_ref() == Some(&intent.target)),
            "worker restart target changed before admission"
        );
        tx.execute(
            "INSERT INTO worker_restart_intents(session_id,operation_id,target_json,desired_build,phase)
             VALUES (?1,?2,?3,?4,'prepared') ON CONFLICT(session_id) DO UPDATE SET
             operation_id=excluded.operation_id,target_json=excluded.target_json,
             desired_build=excluded.desired_build,phase=excluded.phase",
            params![session_id, intent.operation_id, serde_json::to_string(&intent.target)?, intent.desired_build],
        )?;
        tx.commit()?;
        Ok(())
    })
}

#[derive(Clone, Copy)]
pub(crate) enum WorkerRestartPhase {
    Swapping,
    AwaitingReadiness,
}

impl WorkerRestartPhase {
    fn transition(self) -> (&'static str, &'static str) {
        match self {
            Self::Swapping => ("prepared", "swapping"),
            Self::AwaitingReadiness => ("swapping", "awaiting_readiness"),
        }
    }
}

pub(crate) fn advance_worker_restart(
    session_id: &str,
    operation_id: &str,
    phase: WorkerRestartPhase,
) -> Result<()> {
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

//! A detached worker boot remains recoverable after the daemon releases admission.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRestartIntent {
    pub operation_id: String,
    pub target: TargetLocator,
    pub desired_build: String,
}

pub fn begin_worker_restart(session_id: &str, intent: &WorkerRestartIntent) -> Result<()> {
    begin_worker_restart_for(session_id, intent, false, None)
}

/// The wake and its boot owner become visible together. Recovery must never
/// see a still-parked child whose worker has already accepted launch.
pub fn begin_subagent_unpark(
    session_id: &str,
    intent: &WorkerRestartIntent,
    profile_change: Option<(&str, &str)>,
) -> Result<()> {
    begin_worker_restart_for(session_id, intent, true, profile_change)
}

fn begin_worker_restart_for(
    session_id: &str,
    intent: &WorkerRestartIntent,
    unpark: bool,
    profile_change: Option<(&str, &str)>,
) -> Result<()> {
    let _owner = crate::worker_lifecycle::require(session_id)?;
    let session_id = session_id.to_owned();
    let intent = intent.clone();
    let profile_change =
        profile_change.map(|(previous, next)| (previous.to_owned(), next.to_owned()));
    submit_database_write("begin_worker_restart", move |connection| {
        let tx = connection.transaction()?;
        let record = state_io::load_session_with(&tx, &session_id)?;
        ensure!(
            record.is_some_and(|s| s.target.as_deref() == Some(&intent.target)),
            "worker restart target changed before admission"
        );
        if unpark {
            super::session_restart::ensure_container_parent_available(&tx, &session_id)?;
        } else {
            super::session_restart::ensure_no_peer_container_upgrade(&tx, &session_id)?;
        }
        let claimed = tx.execute(
            "INSERT INTO worker_restart_intents(session_id,operation_id,target_json,desired_build,phase)
             VALUES (?1,?2,?3,?4,'prepared') ON CONFLICT(session_id) DO NOTHING",
            params![session_id, intent.operation_id, serde_json::to_string(&intent.target)?, intent.desired_build],
        )?;
        ensure!(
            claimed == 1,
            "another worker replacement already owns session {session_id}"
        );
        if unpark {
            let (previous, next) = profile_change
                .as_ref()
                .map_or((None, None), |(previous, next)| {
                    (Some(previous), Some(next))
                });
            ensure!(
                tx.execute(
                    "UPDATE sessions SET state='running', last_error=NULL, updated_at=?2,
                         last_profile=COALESCE(?3, last_profile)
                     WHERE session_id=?1 AND state='parked'
                       AND (?4 IS NULL OR last_profile=?4)
                       AND EXISTS (SELECT 1 FROM subagent_sessions WHERE child_session_id=?1)",
                    params![session_id, chrono::Utc::now().to_rfc3339(), next, previous],
                )? == 1,
                "only a parked sub-agent can accept a wake"
            );
        }
        tx.commit()?;
        Ok(())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerRestartPhase {
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

pub fn worker_restart_phase(session_id: &str) -> Result<Option<WorkerRestartPhase>> {
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

pub fn advance_worker_restart(
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

pub fn load_worker_restart(session_id: &str) -> Result<Option<WorkerRestartIntent>> {
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

pub fn finish_worker_restart(session_id: &str, operation_id: &str) -> Result<()> {
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

/// A proved dead wake releases its boot and becomes retryable atomically.
pub fn fail_subagent_unpark(
    session_id: &str,
    operation_id: &str,
    detail: &str,
    evidence: Option<&mj_core::subagent::WakeFailure>,
) -> Result<()> {
    let owner = crate::worker_lifecycle::require(session_id)?;
    ensure!(
        owner.operation_id() == operation_id,
        "unpark ownership changed"
    );
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    let failure = match evidence {
        Some(evidence) => evidence.encode(detail)?,
        None => detail.to_owned(),
    };
    submit_database_write("fail_subagent_unpark", move |connection| {
        let tx = connection.transaction()?;
        ensure!(tx.execute(
            "UPDATE sessions SET state='parked', last_error=?3, updated_at=?4
             WHERE session_id=?1 AND state='running'
             AND EXISTS (SELECT 1 FROM worker_restart_intents WHERE session_id=?1 AND operation_id=?2)
             AND EXISTS (SELECT 1 FROM subagent_sessions WHERE child_session_id=?1)",
            params![session_id, operation_id, failure, chrono::Utc::now().to_rfc3339()],
        )? == 1, "failed wake no longer owns the running child");
        finish_worker_restart_with(&tx, &session_id, &operation_id)?;
        tx.commit()?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{session, test_subagent};
    use super::*;
    use crate::recovery_gate::{BackgroundWork, RecoveryGate};
    use crate::worker_lifecycle::WorkerPermit;

    #[test]
    fn worker_restart_and_session_restart_gate_only_same_environment_peers() {
        const CHILD: &str = "MJ_ISOLATED_ENVIRONMENT_PEER_GATES";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            crate::test_isolation::IsolatedTest::new(crate::test_isolation::test_name(
                module_path!(),
                "worker_restart_and_session_restart_gate_only_same_environment_peers",
            ))
            .env(CHILD, "1")
            .isolated_store(directory.path())
            .run();
            return;
        }

        let _writer = install_isolated_test_writer();
        let gate = Arc::new(RecoveryGate::default());
        let mut parent = session("parent-session", "project");
        parent.state = SessionState::Running;
        save_session(&parent).unwrap();
        let mut owned = session("owned-session", "project");
        owned.state = SessionState::Parked;
        let mut owned_relation = test_subagent(&parent.id, &owned.id);
        owned_relation.supervisor = true;
        register_subagent_with_startup(&owned, &owned_relation, Vec::new(), None, None, true)
            .unwrap();
        let mut borrowed = session("borrowed", "project");
        borrowed.state = SessionState::Parked;
        save_subagent_session(&borrowed, &test_subagent(&parent.id, &borrowed.id)).unwrap();
        let mut nested = session("nested", "project");
        nested.state = SessionState::Parked;
        // The owned session has a separate set of borrowers.
        save_subagent_session(&nested, &test_subagent(&owned.id, &nested.id)).unwrap();

        let begin_upgrade = |record: &SessionRecord| {
            let plan = crate::targets::container_upgrade_test_plan(&record.id);
            begin_container_upgrade(&record.id, "upgrade-test", Arc::default(), plan, None)
                .unwrap();
        };
        let commit_upgrade = |record: &SessionRecord| {
            try_commit_container_upgrade(&record.id, "upgrade-test")
                .unwrap()
                .is_committed()
        };
        let restart = |record: &SessionRecord, unpark| {
            let attempt = gate
                .try_start(&record.id, BackgroundWork::WorkerReplacement)
                .unwrap();
            let permit = WorkerPermit::acquire(&record.id, &attempt).unwrap();
            let intent = WorkerRestartIntent {
                operation_id: "test-restart".into(),
                target: record.target.as_deref().unwrap().clone(),
                desired_build: "fixture".into(),
            };
            permit.scope_blocking(|| {
                if unpark {
                    begin_subagent_unpark(&record.id, &intent, None)?;
                } else {
                    begin_worker_restart(&record.id, &intent)?;
                }
                finish_worker_restart(&record.id, &intent.operation_id)
            })
        };

        begin_upgrade(&parent);
        assert!(container_upgrade_owns_target(&borrowed.id).unwrap());
        assert!(!container_upgrade_owns_target(&owned.id).unwrap());
        // A parked worker may wake before commit. The resulting running peer
        // prevents the upgrade from crossing into recreation.
        restart(&borrowed, true).unwrap();
        assert!(!commit_upgrade(&parent));
        save_session(&borrowed).unwrap();
        assert!(commit_upgrade(&parent));
        restart(&parent, false).unwrap();
        assert!(
            restart(&borrowed, true)
                .unwrap_err()
                .to_string()
                .contains("being upgraded")
        );
        assert!(
            restart(&borrowed, false)
                .unwrap_err()
                .to_string()
                .contains("being upgraded")
        );
        restart(&owned, false).unwrap();
        restart(&owned, true).unwrap();
        let mut new_child = session("new-child", "project");
        new_child.target = None;
        let relation = test_subagent(&parent.id, &new_child.id);
        assert!(
            save_subagent_session(&new_child, &relation)
                .unwrap_err()
                .to_string()
                .contains("being upgraded")
        );
        register_subagent_with_startup(&new_child, &relation, Vec::new(), None, None, true)
            .unwrap();
        finish_session_restart(&parent.id, "upgrade-test").unwrap();

        begin_upgrade(&owned);
        assert!(commit_upgrade(&owned));
        assert!(
            restart(&nested, true)
                .unwrap_err()
                .to_string()
                .contains("being upgraded")
        );
        restart(&parent, false).unwrap();
        restart(&borrowed, true).unwrap();
        let mut new_borrower = session("new-borrower", "project");
        new_borrower.target = None;
        save_subagent_session(&new_borrower, &test_subagent(&parent.id, &new_borrower.id)).unwrap();
    }
}

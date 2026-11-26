//! A detached worker boot remains recoverable after the daemon releases admission.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRestartIntent {
    pub operation_id: String,
    pub target: TargetLocator,
    pub desired_build: String,
}

pub fn begin_worker_restart(session_id: &str, intent: &WorkerRestartIntent) -> Result<()> {
    begin_worker_restart_for(session_id, intent, false, None, None)
}

/// The wake and its boot owner become visible together. Recovery must never
/// see a still-parked child whose worker has already accepted launch.
/// A refresh supplies `Some(profile)`, including `Some(None)` to clear the pin.
/// An unchanged staged profile supplies `None`.
pub fn begin_subagent_unpark(
    session_id: &str,
    intent: &WorkerRestartIntent,
    destination: Option<&SessionRecord>,
    staged_aws_profile: Option<Option<&str>>,
) -> Result<()> {
    begin_worker_restart_for(session_id, intent, true, destination, staged_aws_profile)
}

fn begin_worker_restart_for(
    session_id: &str,
    intent: &WorkerRestartIntent,
    unpark: bool,
    destination: Option<&SessionRecord>,
    staged_aws_profile: Option<Option<&str>>,
) -> Result<()> {
    let _owner = crate::worker_lifecycle::require(session_id)?;
    ensure!(
        staged_aws_profile
            .flatten()
            .is_none_or(mj_core::aws_credentials::valid_profile_name),
        "invalid staged AWS profile name"
    );
    let staged_aws_profile = staged_aws_profile.map(|profile| profile.map(str::to_owned));
    let session_id = session_id.to_owned();
    let intent = intent.clone();
    let destination = destination.cloned();
    submit_database_write("begin_worker_restart", move |connection| {
        let tx = connection.transaction()?;
        let record =
            state_io::load_session_with(&tx, &session_id, &state_io::configured_sessions_root())?;
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
            "INSERT INTO worker_restart_intents(session_id,operation_id,target_json,desired_build,phase,staged_aws_profile)
             VALUES (?1,?2,?3,?4,'prepared',?5) ON CONFLICT(session_id) DO NOTHING",
            params![session_id, intent.operation_id, serde_json::to_string(&intent.target)?, intent.desired_build, staged_aws_profile.as_ref().map(|name| name.as_deref().unwrap_or(""))],
        )?;
        ensure!(
            claimed == 1,
            "another worker replacement already owns session {session_id}"
        );
        if unpark {
            ensure!(
                tx.execute(
                    "UPDATE sessions SET state='running', last_error=NULL, updated_at=?2,
                         last_profile=COALESCE(?3, last_profile)
                     WHERE session_id=?1 AND state='parked'
                       AND EXISTS (SELECT 1 FROM subagent_sessions WHERE child_session_id=?1)",
                    params![
                        session_id,
                        chrono::Utc::now().to_rfc3339(),
                        destination.as_ref().map(|record| &record.last_profile)
                    ],
                )? == 1,
                "only a parked sub-agent can accept a wake"
            );
        }
        if let Some(destination) = destination.as_ref() {
            update_unpark_destination(&tx, destination)?;
        }
        tx.commit()?;
        Ok(())
    })
}

fn update_unpark_destination(tx: &Transaction<'_>, record: &SessionRecord) -> Result<()> {
    tx.execute(
        "UPDATE sessions SET last_profile=?2, harness_kind=?3, target_template_id=?4,
         native_session_id=?5, subagents=?6, oracle=?7, oracle_choice=?8 WHERE session_id=?1",
        params![
            record.id,
            record.last_profile,
            record.harness_kind.id(),
            record.target_template_id,
            record.native_session_id,
            record
                .subagents
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            record.oracle,
            record
                .oracle_choice
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?
        ],
    )?;
    Ok(())
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

/// Bind staged files to the existing boot owner before sending the start command.
pub fn record_worker_restart_aws_profile(
    session_id: &str,
    operation_id: &str,
    profile: Option<&str>,
) -> Result<()> {
    let owner = crate::worker_lifecycle::require(session_id)?;
    ensure!(
        owner.operation_id() == operation_id,
        "worker boot ownership changed"
    );
    ensure!(
        profile.is_none_or(mj_core::aws_credentials::valid_profile_name),
        "invalid staged AWS profile name"
    );
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    let profile = profile.unwrap_or("").to_owned();
    submit_database_write("record_worker_restart_aws_profile", move |connection| {
        ensure!(
            connection.execute(
                "UPDATE worker_restart_intents SET staged_aws_profile=?3
             WHERE session_id=?1 AND operation_id=?2 AND phase='swapping'",
                params![session_id, operation_id, profile],
            )? == 1,
            "worker boot no longer owns staged AWS profile"
        );
        Ok(())
    })
}

/// The worker has reached readiness. Publish the staged AWS profile and
/// retire its intent in the same transaction. NULL means no restaging,
/// while an empty name means the activation clears a previous selection.
pub fn activate_worker_restart(session_id: &str, operation_id: &str) -> Result<()> {
    let _owner = crate::worker_lifecycle::require(session_id)?;
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    submit_database_write("activate_worker_restart", move |connection| {
        let tx = connection.transaction()?;
        publish_worker_restart_aws_profile(&tx, &session_id, &operation_id)?;
        finish_worker_restart_with(&tx, &session_id, &operation_id)?;
        tx.commit()?;
        Ok(())
    })
}

pub(super) fn publish_worker_restart_aws_profile(
    tx: &Transaction<'_>,
    session_id: &str,
    operation_id: &str,
) -> Result<()> {
    tx.execute(
        "UPDATE sessions
         SET staged_aws_profile=(SELECT NULLIF(staged_aws_profile, '') FROM worker_restart_intents
                                 WHERE session_id=?1 AND operation_id=?2)
         WHERE session_id=?1 AND EXISTS (
             SELECT 1 FROM worker_restart_intents
             WHERE session_id=?1 AND operation_id=?2 AND staged_aws_profile IS NOT NULL)",
        params![session_id, operation_id],
    )?;
    Ok(())
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

pub enum SubagentUnparkFailure<'a> {
    Retryable {
        detail: &'a str,
        evidence: Option<&'a mj_core::subagent::WakeFailure>,
    },
    WorkingDirectoryUnavailable(&'a str),
}

/// Settle a failed wake and its boot intent in one transaction.
pub fn fail_subagent_unpark(
    session_id: &str,
    operation_id: &str,
    failure: SubagentUnparkFailure<'_>,
) -> Result<()> {
    fail_subagent_unpark_with_rollback(session_id, operation_id, failure, None)
}

pub fn fail_subagent_unpark_with_rollback(
    session_id: &str,
    operation_id: &str,
    failure: SubagentUnparkFailure<'_>,
    rollback: Option<&SessionRecord>,
) -> Result<()> {
    let rollback = rollback.cloned();
    let owner = crate::worker_lifecycle::require(session_id)?;
    ensure!(
        owner.operation_id() == operation_id,
        "unpark ownership changed"
    );
    let session_id = session_id.to_owned();
    let operation_id = operation_id.to_owned();
    let (state, detail) = match failure {
        SubagentUnparkFailure::Retryable { detail, evidence } => (
            SessionState::Parked,
            match evidence {
                Some(evidence) => evidence.encode(detail)?,
                None => detail.to_owned(),
            },
        ),
        SubagentUnparkFailure::WorkingDirectoryUnavailable(detail) => {
            (SessionState::Error, detail.to_owned())
        }
    };
    submit_database_write("fail_subagent_unpark", move |connection| {
        let tx = connection.transaction()?;
        ensure!(tx.execute(
            "UPDATE sessions SET state=?5, last_error=?3, updated_at=?4
             WHERE session_id=?1 AND state='running'
             AND EXISTS (SELECT 1 FROM worker_restart_intents WHERE session_id=?1 AND operation_id=?2)
             AND EXISTS (SELECT 1 FROM subagent_sessions WHERE child_session_id=?1)",
            params![session_id, operation_id, detail, chrono::Utc::now().to_rfc3339(), state.as_str()],
        )? == 1, "failed wake no longer owns the running child");
        if state == SessionState::Parked
            && let Some(record) = rollback.as_ref()
        {
            update_unpark_destination(&tx, record)?;
        }
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
        owned_relation.orchestrator = true;
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
                    begin_subagent_unpark(&record.id, &intent, None, Some(Some("team")))?;
                } else {
                    begin_worker_restart(&record.id, &intent)?;
                }
                activate_worker_restart(&record.id, &intent.operation_id)
            })
        };

        begin_upgrade(&parent);
        assert!(container_upgrade_owns_target(&borrowed.id).unwrap());
        assert!(!container_upgrade_owns_target(&owned.id).unwrap());
        // A parked worker may wake before commit. The resulting running peer
        // prevents the upgrade from crossing into recreation.
        restart(&borrowed, true).unwrap();
        assert_eq!(
            load_state().unwrap().sessions[&borrowed.id]
                .staged_aws_profile
                .as_deref(),
            Some("team")
        );
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

//! Semantic database bodies retain the exact internal turn for existing clients.
use super::*;
use mj_core::event_outcome::ApiTurnOutcome;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct StoredTurnOutcome {
    #[serde(flatten)]
    semantic: ApiTurnOutcome,
    raw_outcome: MaterializedTurnOutcome,
}

pub(super) fn serialize_turn_outcome(turn: &MaterializedTurnOutcome) -> Result<String> {
    Ok(serde_json::to_string(&StoredTurnOutcome {
        semantic: ApiTurnOutcome::from(turn),
        raw_outcome: turn.clone(),
    })?)
}

pub(super) fn deserialize_turn_outcome(body: &str) -> Result<MaterializedTurnOutcome> {
    #[derive(Deserialize)]
    struct RawTurnOutcome {
        raw_outcome: MaterializedTurnOutcome,
    }
    Ok(serde_json::from_str::<RawTurnOutcome>(body)?.raw_outcome)
}

pub(super) fn migrate_turn_outcomes(tx: &Transaction<'_>) -> Result<()> {
    let mut query = tx.prepare("SELECT session_id, command_id, body FROM session_turn_usage")?;
    let rows = query
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (session, command, body) in rows {
        let turn: MaterializedTurnOutcome = serde_json::from_str(&body)
            .with_context(|| format!("decode legacy session_turn_usage for {session}/{command}"))?;
        tx.execute(
            "UPDATE session_turn_usage SET body=?3 WHERE session_id=?1 AND command_id=?2",
            params![session, command, serialize_turn_outcome(&turn)?],
        )?;
    }
    let mut query = tx.prepare("SELECT session_id, last_turn_outcome_json FROM materialized_sessions WHERE last_turn_outcome_json IS NOT NULL")?;
    let rows = query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (session, body) in rows {
        let turn: MaterializedTurnOutcome = serde_json::from_str(&body)
            .with_context(|| format!("decode legacy materialized last turn for {session}"))?;
        tx.execute(
            "UPDATE materialized_sessions SET last_turn_outcome_json=?2 WHERE session_id=?1",
            params![session, serialize_turn_outcome(&turn)?],
        )?;
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use mj_core::event_outcome::{OutcomeReason, TurnResultKind};
    use mj_core::state::TurnOutcomeKind;

    pub(crate) fn legacy_turns() -> Vec<MaterializedTurnOutcome> {
        let mut outcomes = [
            "EndTurn",
            "Cancelled",
            "error",
            "awaiting_input",
            "QuotaLimit",
            "prompt_unanswered",
            "ModelCapacity",
            "Refusal",
        ]
        .into_iter()
        .map(|stop_reason| TurnOutcomeKind::Completed {
            stop_reason: stop_reason.into(),
        })
        .collect::<Vec<_>>();
        for reason in [
            None,
            Some(OutcomeReason::LegacyUnclassified),
            Some(OutcomeReason::RequestedCancellation),
            Some(OutcomeReason::WorkerRestarted),
        ] {
            outcomes.push(TurnOutcomeKind::Interrupted {
                message: "original interruption".into(),
                reason,
            });
        }
        for reason in [
            None,
            Some(OutcomeReason::AdmissionRejected),
            Some(OutcomeReason::RuntimeFailure),
        ] {
            outcomes.push(TurnOutcomeKind::Rejected {
                message: "original rejection".into(),
                reason,
            });
        }
        outcomes.into_iter().enumerate().flat_map(|(index, outcome)| {
            [false, true].map(|diagnostic| {
                serde_json::from_value(serde_json::json!({
                    "command_id": format!("turn-{index}-{diagnostic}"),
                    "accepted_ordinal": index + 1, "turn_start_position": index + 1,
                    "completed_ordinal": index + 2, "completed_at_ms": 100,
                    "outcome": outcome,
                    "diagnostic": diagnostic.then(|| serde_json::json!({"message": "different provider diagnostic", "code": "service"})),
                    "usage": {"scope": "turn", "input_tokens": 42, "output_tokens": 10, "total_tokens": 52}
                })).unwrap()
            })
        }).collect()
    }

    #[test]
    fn stored_semantic_turns_preserve_raw_reasons_and_diagnostics_exactly() -> Result<()> {
        for turn in legacy_turns() {
            let body = serialize_turn_outcome(&turn)?;
            assert_eq!(deserialize_turn_outcome(&body)?, turn);
            assert!(serde_json::from_str::<MaterializedTurnOutcome>(&body).is_err());
            let semantic: ApiTurnOutcome = serde_json::from_str(&body)?;
            assert_eq!(semantic, ApiTurnOutcome::from(&turn));
            if matches!(&turn.outcome, TurnOutcomeKind::Completed { stop_reason } if stop_reason == "Cancelled")
            {
                assert_eq!(semantic.outcome.kind, TurnResultKind::Cancelled);
            }
        }
        Ok(())
    }

    #[test]
    fn turn_migration_rolls_back_both_columns_and_revision_on_invalid_body() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("rollback.sqlite3");
        super::super::save_session_to(&path, &super::super::tests::session("session", "project"))?;
        let connection = Connection::open(&path)?;
        let legacy = serde_json::to_string(&legacy_turns()[0])?;
        connection.execute(
            "INSERT INTO session_turn_usage VALUES ('session','turn',2,1,?1)",
            [&legacy],
        )?;
        connection.execute("UPDATE materialized_sessions SET last_turn_outcome_json='{}' WHERE session_id='session'", [])?;
        connection.execute_batch("ALTER TABLE subagent_sessions DROP COLUMN notification_epoch; DELETE FROM schema_migrations WHERE version>=82; UPDATE schema_compatibility SET minimum_compatible_version=81; PRAGMA user_version=81;")?;
        let error = super::super::schema::migrate_turn_outcome_schema(&connection).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("materialized last turn for session")
        );
        assert_eq!(
            connection.query_row("SELECT body FROM session_turn_usage", [], |row| row
                .get::<_, String>(0))?,
            legacy
        );
        assert_eq!(
            connection.query_row(
                "SELECT last_turn_outcome_json FROM materialized_sessions",
                [],
                |row| row.get::<_, String>(0)
            )?,
            "{}"
        );
        assert_eq!(
            super::super::schema::read_schema_state(&connection)?.revision,
            81
        );
        assert_eq!(
            super::super::schema::read_schema_state(&connection)?.minimum_compatible,
            Some(81)
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires MJ_TURN_OUTCOME_COPY pointing to an isolated read-only-source table copy"]
    fn migrate_copied_live_turn_outcomes() -> Result<()> {
        let path = std::env::var("MJ_TURN_OUTCOME_COPY")?;
        mj_core::config::ensure_may_control_store(
            Path::new(&path)
                .parent()
                .context("copied database needs a parent directory")?,
            "migrate the copied turn-outcome database",
        )?;
        let connection = Connection::open(&path)?;
        let before = copied_rows(&connection)?;
        super::super::schema::migrate_turn_outcome_schema(&connection)?;
        let after = copied_rows(&connection)?;
        assert_eq!(before.len(), after.len());
        let mut absent_reasons = 0;
        let mut diagnostics = 0;
        for ((before_key, body), (after_key, migrated)) in before.into_iter().zip(after) {
            assert_eq!(before_key, after_key);
            let original: MaterializedTurnOutcome = serde_json::from_str(&body)?;
            assert_eq!(
                deserialize_turn_outcome(&migrated)?,
                original,
                "{before_key}"
            );
            assert_eq!(
                serde_json::from_str::<ApiTurnOutcome>(&migrated)?,
                ApiTurnOutcome::from(&original)
            );
            absent_reasons += usize::from(matches!(
                original.outcome,
                TurnOutcomeKind::Interrupted { reason: None, .. }
            ));
            diagnostics += usize::from(original.diagnostic.is_some());
        }
        println!(
            "Verified all copied turns exactly; absent interruption reasons={absent_reasons}, diagnostics={diagnostics}"
        );
        assert_eq!(
            super::super::schema::read_schema_state(&connection)?.revision,
            82
        );
        Ok(())
    }

    fn copied_rows(connection: &Connection) -> Result<Vec<(String, String)>> {
        Ok(connection.prepare("SELECT 'usage:' || session_id || '/' || command_id, body FROM session_turn_usage UNION ALL SELECT 'last:' || session_id, last_turn_outcome_json FROM materialized_sessions WHERE last_turn_outcome_json IS NOT NULL ORDER BY 1")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }
}

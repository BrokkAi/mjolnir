use super::*;
use crate::hel_usage::ProviderCost;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCoverage {
    pub recorded_turns: u64,
    pub full_turn_reports: u64,
    pub last_request_reports: u64,
    pub unspecified_reports: u64,
    pub missing_reports: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCounterTotal {
    pub tokens: u64,
    /// Number of full-turn reports supplying this particular counter.
    pub reported_turns: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsagePage {
    pub session_id: String,
    pub turns: Vec<MaterializedTurnOutcome>,
    pub next_after_seq: u64,
    pub latest_seq: u64,
    /// Totals include only reports whose scope is known to be a whole turn.
    pub totals: BTreeMap<String, UsageCounterTotal>,
    pub coverage: UsageCoverage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_cost: Option<ProviderCost>,
}

pub fn load_session_usage(
    session_id: &str,
    after_seq: u64,
    limit: usize,
) -> Result<Option<UsagePage>> {
    load_session_usage_from(&database_path(), session_id, after_seq, limit)
}

fn load_session_usage_from(
    path: &Path,
    session_id: &str,
    after_seq: u64,
    limit: usize,
) -> Result<Option<UsagePage>> {
    let mut connection = open_reader(path)?;
    let tx = connection.transaction()?;
    if read_materialized_session_fields(&tx, session_id)?.is_none() {
        return Ok(None);
    }
    let latest_seq: u64 = tx.query_row(
        "SELECT COALESCE(MAX(completed_ordinal), 0) FROM session_turn_usage WHERE session_id = ?1",
        [session_id],
        |r| r.get(0),
    )?;
    let mut statement = tx.prepare("SELECT body FROM session_turn_usage WHERE session_id = ?1 AND completed_ordinal > ?2 ORDER BY completed_ordinal LIMIT ?3")?;
    let turns = statement
        .query_map(
            params![session_id, after_seq, limit.clamp(1, 1000) as i64],
            |r| r.get::<_, String>(0),
        )?
        .map(|r| Ok(serde_json::from_str::<MaterializedTurnOutcome>(&r?)?))
        .collect::<Result<Vec<_>>>()?;
    let mut coverage = UsageCoverage::default();
    let mut statement = tx.prepare("SELECT json_extract(body, '$.usage.scope'), COUNT(*) FROM session_turn_usage WHERE session_id = ?1 GROUP BY json_extract(body, '$.usage.scope')")?;
    for row in statement.query_map([session_id], |r| {
        Ok((r.get::<_, Option<String>>(0)?, r.get::<_, u64>(1)?))
    })? {
        let (scope, count) = row?;
        coverage.recorded_turns += count;
        match scope.as_deref() {
            Some("turn") => coverage.full_turn_reports += count,
            Some("last_request") => coverage.last_request_reports += count,
            Some(_) => coverage.unspecified_reports += count,
            None => coverage.missing_reports += count,
        }
    }
    let mut totals = BTreeMap::new();
    for counter in [
        "total_tokens",
        "input_tokens",
        "output_tokens",
        "thought_tokens",
        "cached_read_tokens",
        "cached_write_tokens",
    ] {
        let (tokens, reported_turns) = tx.query_row("SELECT SUM(json_extract(body, ?2)), COUNT(json_extract(body, ?2)) FROM session_turn_usage WHERE session_id = ?1 AND json_extract(body, '$.usage.scope') = 'turn'", params![session_id, format!("$.usage.{counter}")], |r| Ok((r.get::<_, Option<u64>>(0)?, r.get::<_, u64>(1)?)))?;
        if let Some(tokens) = tokens {
            totals.insert(
                counter.into(),
                UsageCounterTotal {
                    tokens,
                    reported_turns,
                },
            );
        }
    }
    let cost: Option<String> = tx
        .query_row(
            "SELECT body FROM session_provider_cost WHERE session_id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(Some(UsagePage {
        session_id: session_id.into(),
        next_after_seq: turns
            .last()
            .map_or(latest_seq.max(after_seq), |t| t.completed_ordinal),
        latest_seq,
        turns,
        totals,
        coverage,
        provider_session_cost: cost.map(|s| serde_json::from_str(&s)).transpose()?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hel_state::TurnOutcomeKind;
    use crate::hel_usage::{TokenUsage, UsageScope};

    #[test]
    fn usage_survives_reopening_and_replay_with_honest_coverage() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("usage.sqlite");
        let conn = open(&path)?;
        // Use the same projection setup as the materialized database tests.
        drop(conn);
        save_session_to(&path, &super::super::tests::session("usage", "project"))?;

        let mut turns = Vec::new();
        for (i, scope) in [
            Some(UsageScope::Turn),
            Some(UsageScope::Turn),
            Some(UsageScope::LastRequest),
            None,
        ]
        .into_iter()
        .enumerate()
        {
            let turn = MaterializedTurnOutcome {
                command_id: format!("p{i}"),
                accepted_ordinal: None,
                turn_start_position: Some(i as u64 + 1),
                completed_ordinal: i as u64 + 1,
                completed_at_ms: 100,
                outcome: TurnOutcomeKind::Completed {
                    stop_reason: "EndTurn".into(),
                },
                usage: scope.map(|scope| TokenUsage {
                    scope,
                    total_tokens: 30,
                    input_tokens: 20,
                    output_tokens: 10,
                    thought_tokens: None,
                    cached_read_tokens: Some(0),
                    cached_write_tokens: None,
                }),
            };
            turns.push(turn);
        }
        for _ in 0..2 {
            apply_projection_page_to(&path, "usage", |page| {
                for turn in &turns {
                    let prior = if turn.completed_ordinal == 1 {
                        crate::hel_worker::RELAY_EVENT_GENESIS_DIGEST.into()
                    } else {
                        format!("{:064x}", turn.completed_ordinal - 1)
                    };
                    page.apply(
                        turn.completed_ordinal,
                        &prior,
                        &format!("{:064x}", turn.completed_ordinal),
                        &MaterializedSessionMutation {
                            last_turn_outcome: Some(turn.clone()),
                            provider_cost: Some(ProviderCost {
                                amount: 1.25,
                                currency: "USD".into(),
                                observed_at_ms: 100,
                            }),
                            ..Default::default()
                        },
                    )?;
                }
                Ok(())
            })?;
        }
        let first = load_session_usage_from(&path, "usage", 0, 2)?.unwrap();
        assert_eq!(first.turns.len(), 2);
        assert_eq!(first.provider_session_cost.as_ref().unwrap().amount, 1.25);
        assert_eq!(first.coverage.recorded_turns, 4);
        assert_eq!(first.coverage.full_turn_reports, 2);
        assert_eq!(first.coverage.last_request_reports, 1);
        assert_eq!(first.coverage.missing_reports, 1);
        assert_eq!(first.totals["total_tokens"].tokens, 60);
        assert_eq!(first.totals["cached_read_tokens"].tokens, 0);
        assert!(!first.totals.contains_key("thought_tokens"));
        let next = load_session_usage_from(&path, "usage", first.next_after_seq, 2)?.unwrap();
        assert_eq!(next.turns.len(), 2);
        assert_eq!(next.next_after_seq, next.latest_seq);
        assert_eq!(next.totals, first.totals);
        Ok(())
    }
}
